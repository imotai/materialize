// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! TBD: Currently, `sql::func` handles matching arguments to their respective
//! built-in functions (for most built-in functions, at least).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::LazyLock;

use itertools::Itertools;
use mz_expr::func;
use mz_ore::collections::CollectionExt;
use mz_ore::str::StrExt;
use mz_pgrepr::oid;
use mz_repr::role_id::RoleId;
use mz_repr::{ColumnName, Datum, RelationType, ScalarBaseType, ScalarType};

use crate::ast::{SelectStatement, Statement};
use crate::catalog::{CatalogType, TypeCategory, TypeReference};
use crate::names::{self, ResolvedItemName};
use crate::plan::error::PlanError;
use crate::plan::hir::{
    AggregateFunc, BinaryFunc, CoercibleScalarExpr, CoercibleScalarType, ColumnOrder,
    HirRelationExpr, HirScalarExpr, ScalarWindowFunc, TableFunc, UnaryFunc, UnmaterializableFunc,
    ValueWindowFunc, VariadicFunc,
};
use crate::plan::query::{self, ExprContext, QueryContext};
use crate::plan::scope::Scope;
use crate::plan::side_effecting_func::PG_CATALOG_SEF_BUILTINS;
use crate::plan::transform_ast;
use crate::plan::typeconv::{self, CastContext};
use crate::session::vars::{self, ENABLE_TIME_AT_TIME_ZONE};

/// A specifier for a function or an operator.
#[derive(Clone, Copy, Debug)]
pub enum FuncSpec<'a> {
    /// A function name.
    Func(&'a ResolvedItemName),
    /// An operator name.
    Op(&'a str),
}

impl<'a> fmt::Display for FuncSpec<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FuncSpec::Func(n) => n.fmt(f),
            FuncSpec::Op(o) => o.fmt(f),
        }
    }
}

impl TypeCategory {
    /// Extracted from PostgreSQL 9.6.
    /// ```sql,ignore
    /// SELECT array_agg(typname), typcategory
    /// FROM pg_catalog.pg_type
    /// WHERE typname IN (
    ///  'bool', 'bytea', 'date', 'float4', 'float8', 'int4', 'int8', 'interval', 'jsonb',
    ///  'numeric', 'text', 'time', 'timestamp', 'timestamptz'
    /// )
    /// GROUP BY typcategory
    /// ORDER BY typcategory;
    /// ```
    pub fn from_type(typ: &ScalarType) -> Self {
        // Keep this in sync with `from_catalog_type`.
        match typ {
            ScalarType::Array(..) | ScalarType::Int2Vector => Self::Array,
            ScalarType::Bool => Self::Boolean,
            ScalarType::AclItem
            | ScalarType::Bytes
            | ScalarType::Jsonb
            | ScalarType::Uuid
            | ScalarType::MzAclItem => Self::UserDefined,
            ScalarType::Date
            | ScalarType::Time
            | ScalarType::Timestamp { .. }
            | ScalarType::TimestampTz { .. } => Self::DateTime,
            ScalarType::Float32
            | ScalarType::Float64
            | ScalarType::Int16
            | ScalarType::Int32
            | ScalarType::Int64
            | ScalarType::UInt16
            | ScalarType::UInt32
            | ScalarType::UInt64
            | ScalarType::Oid
            | ScalarType::RegClass
            | ScalarType::RegProc
            | ScalarType::RegType
            | ScalarType::Numeric { .. } => Self::Numeric,
            ScalarType::Interval => Self::Timespan,
            ScalarType::List { .. } => Self::List,
            ScalarType::PgLegacyChar
            | ScalarType::PgLegacyName
            | ScalarType::String
            | ScalarType::Char { .. }
            | ScalarType::VarChar { .. } => Self::String,
            ScalarType::Record { custom_id, .. } => {
                if custom_id.is_some() {
                    Self::Composite
                } else {
                    Self::Pseudo
                }
            }
            ScalarType::Map { .. } => Self::Pseudo,
            ScalarType::MzTimestamp => Self::Numeric,
            ScalarType::Range { .. } => Self::Range,
        }
    }

    pub fn from_param(param: &ParamType) -> Self {
        match param {
            ParamType::Any
            | ParamType::AnyElement
            | ParamType::ArrayAny
            | ParamType::ArrayAnyCompatible
            | ParamType::AnyCompatible
            | ParamType::ListAny
            | ParamType::ListAnyCompatible
            | ParamType::ListElementAnyCompatible
            | ParamType::Internal
            | ParamType::NonVecAny
            | ParamType::NonVecAnyCompatible
            | ParamType::MapAny
            | ParamType::MapAnyCompatible
            | ParamType::RecordAny => Self::Pseudo,
            ParamType::RangeAnyCompatible | ParamType::RangeAny => Self::Range,
            ParamType::Plain(t) => Self::from_type(t),
        }
    }

    /// Like [`TypeCategory::from_type`], but for catalog types.
    // TODO(benesch): would be nice to figure out how to share code with
    // `from_type`, but the refactor to enable that would be substantial.
    pub fn from_catalog_type<T>(catalog_type: &CatalogType<T>) -> Self
    where
        T: TypeReference,
    {
        // Keep this in sync with `from_type`.
        match catalog_type {
            CatalogType::Array { .. } | CatalogType::Int2Vector => Self::Array,
            CatalogType::Bool => Self::Boolean,
            CatalogType::AclItem
            | CatalogType::Bytes
            | CatalogType::Jsonb
            | CatalogType::Uuid
            | CatalogType::MzAclItem => Self::UserDefined,
            CatalogType::Date
            | CatalogType::Time
            | CatalogType::Timestamp
            | CatalogType::TimestampTz => Self::DateTime,
            CatalogType::Float32
            | CatalogType::Float64
            | CatalogType::Int16
            | CatalogType::Int32
            | CatalogType::Int64
            | CatalogType::UInt16
            | CatalogType::UInt32
            | CatalogType::UInt64
            | CatalogType::Oid
            | CatalogType::RegClass
            | CatalogType::RegProc
            | CatalogType::RegType
            | CatalogType::Numeric { .. } => Self::Numeric,
            CatalogType::Interval => Self::Timespan,
            CatalogType::List { .. } => Self::List,
            CatalogType::PgLegacyChar
            | CatalogType::PgLegacyName
            | CatalogType::String
            | CatalogType::Char { .. }
            | CatalogType::VarChar { .. } => Self::String,
            CatalogType::Record { .. } => TypeCategory::Composite,
            CatalogType::Map { .. } | CatalogType::Pseudo => Self::Pseudo,
            CatalogType::MzTimestamp => Self::String,
            CatalogType::Range { .. } => Self::Range,
        }
    }

    /// Extracted from PostgreSQL 9.6.
    /// ```ignore
    /// SELECT typcategory, typname, typispreferred
    /// FROM pg_catalog.pg_type
    /// WHERE typispreferred = true
    /// ORDER BY typcategory;
    /// ```
    pub fn preferred_type(&self) -> Option<ScalarType> {
        match self {
            Self::Array
            | Self::BitString
            | Self::Composite
            | Self::Enum
            | Self::Geometric
            | Self::List
            | Self::NetworkAddress
            | Self::Pseudo
            | Self::Range
            | Self::Unknown
            | Self::UserDefined => None,
            Self::Boolean => Some(ScalarType::Bool),
            Self::DateTime => Some(ScalarType::TimestampTz { precision: None }),
            Self::Numeric => Some(ScalarType::Float64),
            Self::String => Some(ScalarType::String),
            Self::Timespan => Some(ScalarType::Interval),
        }
    }
}

/// Builds an expression that evaluates a scalar function on the provided
/// input expressions.
pub struct Operation<R>(
    pub  Box<
        dyn Fn(
                &ExprContext,
                Vec<CoercibleScalarExpr>,
                &ParamList,
                Vec<ColumnOrder>,
            ) -> Result<R, PlanError>
            + Send
            + Sync,
    >,
);

impl<R> fmt::Debug for Operation<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Operation").finish()
    }
}

impl Operation<HirScalarExpr> {
    /// Builds a unary operation that simply returns its input.
    fn identity() -> Operation<HirScalarExpr> {
        Operation::unary(|_ecx, e| Ok(e))
    }
}

impl<R> Operation<R> {
    fn new<F>(f: F) -> Operation<R>
    where
        F: Fn(
                &ExprContext,
                Vec<CoercibleScalarExpr>,
                &ParamList,
                Vec<ColumnOrder>,
            ) -> Result<R, PlanError>
            + Send
            + Sync
            + 'static,
    {
        Operation(Box::new(f))
    }

    /// Builds an operation that takes no arguments.
    fn nullary<F>(f: F) -> Operation<R>
    where
        F: Fn(&ExprContext) -> Result<R, PlanError> + Send + Sync + 'static,
    {
        Self::variadic(move |ecx, exprs| {
            assert!(exprs.is_empty());
            f(ecx)
        })
    }

    /// Builds an operation that takes one argument.
    fn unary<F>(f: F) -> Operation<R>
    where
        F: Fn(&ExprContext, HirScalarExpr) -> Result<R, PlanError> + Send + Sync + 'static,
    {
        Self::variadic(move |ecx, exprs| f(ecx, exprs.into_element()))
    }

    /// Builds an operation that takes one argument and an order_by.
    fn unary_ordered<F>(f: F) -> Operation<R>
    where
        F: Fn(&ExprContext, HirScalarExpr, Vec<ColumnOrder>) -> Result<R, PlanError>
            + Send
            + Sync
            + 'static,
    {
        Self::new(move |ecx, cexprs, params, order_by| {
            let exprs = coerce_args_to_types(ecx, cexprs, params)?;
            f(ecx, exprs.into_element(), order_by)
        })
    }

    /// Builds an operation that takes two arguments.
    fn binary<F>(f: F) -> Operation<R>
    where
        F: Fn(&ExprContext, HirScalarExpr, HirScalarExpr) -> Result<R, PlanError>
            + Send
            + Sync
            + 'static,
    {
        Self::variadic(move |ecx, exprs| {
            assert_eq!(exprs.len(), 2);
            let mut exprs = exprs.into_iter();
            let left = exprs.next().unwrap();
            let right = exprs.next().unwrap();
            f(ecx, left, right)
        })
    }

    /// Builds an operation that takes two arguments and an order_by.
    ///
    /// If returning an aggregate function, it should return `true` for
    /// [`AggregateFunc::is_order_sensitive`].
    fn binary_ordered<F>(f: F) -> Operation<R>
    where
        F: Fn(&ExprContext, HirScalarExpr, HirScalarExpr, Vec<ColumnOrder>) -> Result<R, PlanError>
            + Send
            + Sync
            + 'static,
    {
        Self::new(move |ecx, cexprs, params, order_by| {
            let exprs = coerce_args_to_types(ecx, cexprs, params)?;
            assert_eq!(exprs.len(), 2);
            let mut exprs = exprs.into_iter();
            let left = exprs.next().unwrap();
            let right = exprs.next().unwrap();
            f(ecx, left, right, order_by)
        })
    }

    /// Builds an operation that takes any number of arguments.
    fn variadic<F>(f: F) -> Operation<R>
    where
        F: Fn(&ExprContext, Vec<HirScalarExpr>) -> Result<R, PlanError> + Send + Sync + 'static,
    {
        Self::new(move |ecx, cexprs, params, _order_by| {
            let exprs = coerce_args_to_types(ecx, cexprs, params)?;
            f(ecx, exprs)
        })
    }
}

/// Backing implementation for sql_impl_func and sql_impl_cast. See those
/// functions for details.
pub fn sql_impl(
    expr: &str,
) -> impl Fn(&QueryContext, Vec<ScalarType>) -> Result<HirScalarExpr, PlanError> + use<> {
    let expr = mz_sql_parser::parser::parse_expr(expr).unwrap_or_else(|e| {
        panic!(
            "static function definition failed to parse {}: {}",
            expr.quoted(),
            e,
        )
    });
    move |qcx, types| {
        // Reconstruct an expression context where the parameter types are
        // bound to the types of the expressions in `args`.
        let mut scx = qcx.scx.clone();
        scx.param_types = RefCell::new(
            types
                .into_iter()
                .enumerate()
                .map(|(i, ty)| (i + 1, ty))
                .collect(),
        );
        let qcx = QueryContext::root(&scx, qcx.lifetime);

        let (mut expr, _) = names::resolve(qcx.scx.catalog, expr.clone())?;
        // Desugar the expression
        transform_ast::transform(&scx, &mut expr)?;

        let ecx = ExprContext {
            qcx: &qcx,
            name: "static function definition",
            scope: &Scope::empty(),
            relation_type: &RelationType::empty(),
            allow_aggregates: false,
            allow_subqueries: true,
            allow_parameters: true,
            allow_windows: false,
        };

        // Plan the expression.
        query::plan_expr(&ecx, &expr)?.type_as_any(&ecx)
    }
}

// Constructs a definition for a built-in function out of a static SQL
// expression.
//
// The SQL expression should use the standard parameter syntax (`$1`, `$2`, ...)
// to refer to the inputs to the function. For example, a built-in function that
// takes two arguments and concatenates them with an arrow in between could be
// defined like so:
//
//     sql_impl_func("$1 || '<->' || $2")
//
// The number of parameters in the SQL expression must exactly match the number
// of parameters in the built-in's declaration. There is no support for variadic
// functions.
fn sql_impl_func(expr: &str) -> Operation<HirScalarExpr> {
    let invoke = sql_impl(expr);
    Operation::variadic(move |ecx, args| {
        let types = args.iter().map(|arg| ecx.scalar_type(arg)).collect();
        let mut out = invoke(ecx.qcx, types)?;
        out.splice_parameters(&args, 0);
        Ok(out)
    })
}

// Defines a built-in table function from a static SQL SELECT statement.
//
// The SQL statement should use the standard parameter syntax (`$1`, `$2`, ...)
// to refer to the inputs to the function; see sql_impl_func for an example.
//
// The number of parameters in the SQL expression must exactly match the number
// of parameters in the built-in's declaration. There is no support for variadic
// functions.
//
// As this is a full SQL statement, it returns a set of rows, similar to a
// table function. The SELECT's projection's names are used and should be
// aliased if needed.
fn sql_impl_table_func_inner(
    sql: &'static str,
    feature_flag: Option<&'static vars::FeatureFlag>,
) -> Operation<TableFuncPlan> {
    let query = match mz_sql_parser::parser::parse_statements(sql)
        .expect("static function definition failed to parse")
        .expect_element(|| "static function definition must have exactly one statement")
        .ast
    {
        Statement::Select(SelectStatement { query, as_of: None }) => query,
        _ => panic!("static function definition expected SELECT statement"),
    };
    let invoke = move |qcx: &QueryContext, types: Vec<ScalarType>| {
        // Reconstruct an expression context where the parameter types are
        // bound to the types of the expressions in `args`.
        let mut scx = qcx.scx.clone();
        scx.param_types = RefCell::new(
            types
                .into_iter()
                .enumerate()
                .map(|(i, ty)| (i + 1, ty))
                .collect(),
        );
        let mut qcx = QueryContext::root(&scx, qcx.lifetime);

        let query = query.clone();
        let (mut query, _) = names::resolve(qcx.scx.catalog, query)?;
        transform_ast::transform(&scx, &mut query)?;

        query::plan_nested_query(&mut qcx, &query)
    };

    Operation::variadic(move |ecx, args| {
        if let Some(feature_flag) = feature_flag {
            ecx.require_feature_flag(feature_flag)?;
        }
        let types = args.iter().map(|arg| ecx.scalar_type(arg)).collect();
        let (mut expr, scope) = invoke(ecx.qcx, types)?;
        expr.splice_parameters(&args, 0);
        Ok(TableFuncPlan {
            expr,
            column_names: scope.column_names().cloned().collect(),
        })
    })
}

fn sql_impl_table_func(sql: &'static str) -> Operation<TableFuncPlan> {
    sql_impl_table_func_inner(sql, None)
}

fn experimental_sql_impl_table_func(
    feature: &'static vars::FeatureFlag,
    sql: &'static str,
) -> Operation<TableFuncPlan> {
    sql_impl_table_func_inner(sql, Some(feature))
}

/// Describes a single function's implementation.
pub struct FuncImpl<R> {
    pub oid: u32,
    pub params: ParamList,
    pub return_type: ReturnType,
    pub op: Operation<R>,
}

/// Describes how each implementation should be represented in the catalog.
#[derive(Debug)]
pub struct FuncImplCatalogDetails {
    pub oid: u32,
    pub arg_typs: Vec<&'static str>,
    pub variadic_typ: Option<&'static str>,
    pub return_typ: Option<&'static str>,
    pub return_is_set: bool,
}

impl<R> FuncImpl<R> {
    pub fn details(&self) -> FuncImplCatalogDetails {
        FuncImplCatalogDetails {
            oid: self.oid,
            arg_typs: self.params.arg_names(),
            variadic_typ: self.params.variadic_name(),
            return_typ: self.return_type.typ.as_ref().map(|t| t.name()),
            return_is_set: self.return_type.is_set_of,
        }
    }
}

impl<R> fmt::Debug for FuncImpl<R> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("FuncImpl")
            .field("oid", &self.oid)
            .field("params", &self.params)
            .field("ret", &self.return_type)
            .field("op", &"<omitted>")
            .finish()
    }
}

impl From<UnmaterializableFunc> for Operation<HirScalarExpr> {
    fn from(n: UnmaterializableFunc) -> Operation<HirScalarExpr> {
        Operation::nullary(move |_ecx| Ok(HirScalarExpr::call_unmaterializable(n.clone())))
    }
}

impl From<UnaryFunc> for Operation<HirScalarExpr> {
    fn from(u: UnaryFunc) -> Operation<HirScalarExpr> {
        Operation::unary(move |_ecx, e| Ok(e.call_unary(u.clone())))
    }
}

impl From<BinaryFunc> for Operation<HirScalarExpr> {
    fn from(b: BinaryFunc) -> Operation<HirScalarExpr> {
        Operation::binary(move |_ecx, left, right| Ok(left.call_binary(right, b.clone())))
    }
}

impl From<VariadicFunc> for Operation<HirScalarExpr> {
    fn from(v: VariadicFunc) -> Operation<HirScalarExpr> {
        Operation::variadic(move |_ecx, exprs| Ok(HirScalarExpr::call_variadic(v.clone(), exprs)))
    }
}

impl From<AggregateFunc> for Operation<(HirScalarExpr, AggregateFunc)> {
    fn from(a: AggregateFunc) -> Operation<(HirScalarExpr, AggregateFunc)> {
        Operation::unary(move |_ecx, e| Ok((e, a.clone())))
    }
}

impl From<ScalarWindowFunc> for Operation<ScalarWindowFunc> {
    fn from(a: ScalarWindowFunc) -> Operation<ScalarWindowFunc> {
        Operation::nullary(move |_ecx| Ok(a.clone()))
    }
}

impl From<ValueWindowFunc> for Operation<(HirScalarExpr, ValueWindowFunc)> {
    fn from(a: ValueWindowFunc) -> Operation<(HirScalarExpr, ValueWindowFunc)> {
        Operation::unary(move |_ecx, e| Ok((e, a.clone())))
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
/// Describes possible types of function parameters.
///
/// Note that this is not exhaustive and will likely require additions.
pub enum ParamList {
    Exact(Vec<ParamType>),
    Variadic {
        leading: Vec<ParamType>,
        trailing: ParamType,
    },
}

impl ParamList {
    /// Determines whether `typs` are compatible with `self`.
    fn matches_argtypes(&self, ecx: &ExprContext, typs: &[CoercibleScalarType]) -> bool {
        if !self.validate_arg_len(typs.len()) {
            return false;
        }

        for (i, typ) in typs.iter().enumerate() {
            let param = &self[i];
            if let CoercibleScalarType::Coerced(typ) = typ {
                // Ensures either `typ` can at least be implicitly cast to a
                // type `param` accepts. Implicit in this check is that unknown
                // type arguments can be cast to any type.
                //
                // N.B. this will require more fallthrough checks once we
                // support RECORD types in functions.
                if !param.accepts_type(ecx, typ) {
                    return false;
                }
            }
        }

        // Ensure a polymorphic solution exists (non-polymorphic functions have
        // trivial polymorphic solutions that evaluate to `None`).
        PolymorphicSolution::new(ecx, typs, self).is_some()
    }

    /// Validates that the number of input elements are viable for `self`.
    fn validate_arg_len(&self, input_len: usize) -> bool {
        match self {
            Self::Exact(p) => p.len() == input_len,
            Self::Variadic { leading, .. } => input_len > leading.len(),
        }
    }

    /// Matches a `&[ScalarType]` derived from the user's function argument
    /// against this `ParamList`'s permitted arguments.
    fn exact_match(&self, types: &[&ScalarType]) -> bool {
        types.iter().enumerate().all(|(i, t)| self[i] == **t)
    }

    /// Generates values underlying data for for `mz_catalog.mz_functions.arg_ids`.
    fn arg_names(&self) -> Vec<&'static str> {
        match self {
            ParamList::Exact(p) => p.iter().map(|p| p.name()).collect::<Vec<_>>(),
            ParamList::Variadic { leading, trailing } => leading
                .iter()
                .chain([trailing])
                .map(|p| p.name())
                .collect::<Vec<_>>(),
        }
    }

    /// Generates values for `mz_catalog.mz_functions.variadic_id`.
    fn variadic_name(&self) -> Option<&'static str> {
        match self {
            ParamList::Exact(_) => None,
            ParamList::Variadic { trailing, .. } => Some(trailing.name()),
        }
    }
}

impl std::ops::Index<usize> for ParamList {
    type Output = ParamType;

    fn index(&self, i: usize) -> &Self::Output {
        match self {
            Self::Exact(p) => &p[i],
            Self::Variadic { leading, trailing } => leading.get(i).unwrap_or(trailing),
        }
    }
}

/// Provides a shorthand function for writing `ParamList::Exact`.
impl From<Vec<ParamType>> for ParamList {
    fn from(p: Vec<ParamType>) -> ParamList {
        ParamList::Exact(p)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
/// Describes parameter types.
///
/// Parameters with "Compatible" in their name are used in conjunction with
/// other "Compatible"-type parameters to determine the best common type to cast
/// arguments to.
///
/// "Compatible" parameters contrast with parameters that contain "Any" in their
/// name, but not "Compatible." These parameters require all other "Any"-type
/// parameters be of the same type from the perspective of
/// [`ScalarType::base_eq`].
///
/// For more details on polymorphic parameter resolution, see `PolymorphicSolution`.
pub enum ParamType {
    /// A pseudotype permitting any type. Note that this parameter does not
    /// enforce the same "Any" constraint as the other "Any"-type parameters.
    Any,
    /// A pseudotype permitting any type, permitting other "Compatibility"-type
    /// parameters to find the best common type.
    AnyCompatible,
    /// An pseudotype permitting any type, requiring other "Any"-type parameters
    /// to be of the same type.
    AnyElement,
    /// An pseudotype permitting any array type, requiring other "Any"-type
    /// parameters to be of the same type.
    ArrayAny,
    /// A pseudotype permitting any array type, permitting other "Compatibility"-type
    /// parameters to find the best common type.
    ArrayAnyCompatible,
    /// An pseudotype permitting any list type, requiring other "Any"-type
    /// parameters to be of the same type.
    ListAny,
    /// A pseudotype permitting any list type, permitting other
    /// "Compatibility"-type parameters to find the best common type.
    ListAnyCompatible,
    /// A pseudotype permitting any type, permitting other "Compatibility"-type
    /// parameters to find the best common type. Additionally, enforces a
    /// constraint that when used with `ListAnyCompatible`, resolves to that
    /// argument's element type.
    ListElementAnyCompatible,
    /// An pseudotype permitting any map type, requiring other "Any"-type
    /// parameters to be of the same type.
    MapAny,
    /// A pseudotype permitting any map type, permitting other "Compatibility"-type
    /// parameters to find the best common type.
    MapAnyCompatible,
    /// A pseudotype permitting any type except `ScalarType::List` and
    /// `ScalarType::Array`, requiring other "Any"-type
    /// parameters to be of the same type.
    NonVecAny,
    /// A pseudotype permitting any type except `ScalarType::List` and
    /// `ScalarType::Array`, requiring other "Compatibility"-type
    /// parameters to be of the same type.
    NonVecAnyCompatible,
    /// A standard parameter that accepts arguments that match its embedded
    /// `ScalarType`.
    Plain(ScalarType),
    /// A polymorphic pseudotype permitting a `ScalarType::Record` of any type,
    /// but all records must be structurally equal.
    RecordAny,
    /// An pseudotype permitting any range type, requiring other "Any"-type
    /// parameters to be of the same type.
    RangeAny,
    /// A pseudotype permitting any range type, permitting other
    /// "Compatibility"-type parameters to find the best common type.
    ///
    /// Prefer using [`ParamType::RangeAny`] over this type; it is easy to fool
    /// this type into generating non-existent range types (e.g. ranges of
    /// floats) that will panic.
    RangeAnyCompatible,
    /// A psuedotype indicating that the function is only meant to be called
    /// internally by the database system.
    Internal,
}

impl ParamType {
    /// Does `self` accept arguments of type `t`?
    fn accepts_type(&self, ecx: &ExprContext, t: &ScalarType) -> bool {
        use ParamType::*;
        use ScalarType::*;

        match self {
            Any | AnyElement | AnyCompatible | ListElementAnyCompatible => true,
            ArrayAny | ArrayAnyCompatible => matches!(t, Array(..) | Int2Vector),
            ListAny | ListAnyCompatible => matches!(t, List { .. }),
            MapAny | MapAnyCompatible => matches!(t, Map { .. }),
            RangeAny | RangeAnyCompatible => matches!(t, Range { .. }),
            NonVecAny | NonVecAnyCompatible => !t.is_vec(),
            Internal => false,
            Plain(to) => typeconv::can_cast(ecx, CastContext::Implicit, t, to),
            RecordAny => matches!(t, Record { .. }),
        }
    }

    /// Does `t`'s [`TypeCategory`] prefer `self`? This question can make
    /// more sense with the understanding that pseudotypes are never preferred.
    fn is_preferred_by(&self, t: &ScalarType) -> bool {
        if let Some(pt) = TypeCategory::from_type(t).preferred_type() {
            *self == pt
        } else {
            false
        }
    }

    /// Is `self` the [`ParamType`] corresponding to `t`'s [near match] value?
    ///
    /// [near match]: ScalarType::near_match
    fn is_near_match(&self, t: &ScalarType) -> bool {
        match (self, t.near_match()) {
            (ParamType::Plain(t), Some(near_match)) => t.structural_eq(near_match),
            _ => false,
        }
    }

    /// Is `self` the preferred parameter type for its `TypeCategory`?
    fn prefers_self(&self) -> bool {
        if let Some(pt) = TypeCategory::from_param(self).preferred_type() {
            *self == pt
        } else {
            false
        }
    }

    fn is_polymorphic(&self) -> bool {
        use ParamType::*;
        match self {
            AnyElement
            | ArrayAny
            | ArrayAnyCompatible
            | AnyCompatible
            | ListAny
            | ListAnyCompatible
            | ListElementAnyCompatible
            | MapAny
            | MapAnyCompatible
            | NonVecAny
            | NonVecAnyCompatible
            // In PG, RecordAny isn't polymorphic even though it offers
            // polymorphic behavior. For more detail, see
            // `PolymorphicCompatClass::StructuralEq`.
            | RecordAny
            | RangeAny
            | RangeAnyCompatible => true,
            Any | Internal | Plain(_)  => false,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            ParamType::Plain(t) => {
                assert!(
                    !t.is_custom_type(),
                    "custom types cannot currently be used as parameters; use a polymorphic parameter that accepts the custom type instead"
                );
                let t: mz_pgrepr::Type = t.into();
                t.catalog_name()
            }
            ParamType::Any => "any",
            ParamType::AnyCompatible => "anycompatible",
            ParamType::AnyElement => "anyelement",
            ParamType::ArrayAny => "anyarray",
            ParamType::ArrayAnyCompatible => "anycompatiblearray",
            ParamType::Internal => "internal",
            ParamType::ListAny => "list",
            ParamType::ListAnyCompatible => "anycompatiblelist",
            // ListElementAnyCompatible is not identical to AnyCompatible, but reusing its ID appears harmless
            ParamType::ListElementAnyCompatible => "anycompatible",
            ParamType::MapAny => "map",
            ParamType::MapAnyCompatible => "anycompatiblemap",
            ParamType::NonVecAny => "anynonarray",
            ParamType::NonVecAnyCompatible => "anycompatiblenonarray",
            ParamType::RecordAny => "record",
            ParamType::RangeAny => "anyrange",
            ParamType::RangeAnyCompatible => "anycompatiblerange",
        }
    }
}

impl PartialEq<ScalarType> for ParamType {
    fn eq(&self, other: &ScalarType) -> bool {
        match self {
            ParamType::Plain(s) => s.base_eq(other),
            // Pseudotypes never equal concrete types
            _ => false,
        }
    }
}

impl PartialEq<ParamType> for ScalarType {
    fn eq(&self, other: &ParamType) -> bool {
        other == self
    }
}

impl From<ScalarType> for ParamType {
    fn from(s: ScalarType) -> ParamType {
        ParamType::Plain(s)
    }
}

impl From<ScalarBaseType> for ParamType {
    fn from(s: ScalarBaseType) -> ParamType {
        use ScalarBaseType::*;
        let s = match s {
            Array | List | Map | Record | Range => {
                panic!("use polymorphic parameters rather than {:?}", s);
            }
            AclItem => ScalarType::AclItem,
            Bool => ScalarType::Bool,
            Int16 => ScalarType::Int16,
            Int32 => ScalarType::Int32,
            Int64 => ScalarType::Int64,
            UInt16 => ScalarType::UInt16,
            UInt32 => ScalarType::UInt32,
            UInt64 => ScalarType::UInt64,
            Float32 => ScalarType::Float32,
            Float64 => ScalarType::Float64,
            Numeric => ScalarType::Numeric { max_scale: None },
            Date => ScalarType::Date,
            Time => ScalarType::Time,
            Timestamp => ScalarType::Timestamp { precision: None },
            TimestampTz => ScalarType::TimestampTz { precision: None },
            Interval => ScalarType::Interval,
            Bytes => ScalarType::Bytes,
            String => ScalarType::String,
            Char => ScalarType::Char { length: None },
            VarChar => ScalarType::VarChar { max_length: None },
            PgLegacyChar => ScalarType::PgLegacyChar,
            PgLegacyName => ScalarType::PgLegacyName,
            Jsonb => ScalarType::Jsonb,
            Uuid => ScalarType::Uuid,
            Oid => ScalarType::Oid,
            RegClass => ScalarType::RegClass,
            RegProc => ScalarType::RegProc,
            RegType => ScalarType::RegType,
            Int2Vector => ScalarType::Int2Vector,
            MzTimestamp => ScalarType::MzTimestamp,
            MzAclItem => ScalarType::MzAclItem,
        };
        ParamType::Plain(s)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct ReturnType {
    pub typ: Option<ParamType>,
    pub is_set_of: bool,
}

impl ReturnType {
    /// Expresses that a function's return type is a scalar value.
    fn scalar(typ: ParamType) -> ReturnType {
        ReturnType {
            typ: Some(typ),
            is_set_of: false,
        }
    }

    /// Expresses that a function's return type is a set of values, e.g. a table
    /// function.
    fn set_of(typ: ParamType) -> ReturnType {
        ReturnType {
            typ: Some(typ),
            is_set_of: true,
        }
    }

    /// Expresses that a function's return type is None.
    fn none(is_set_of: bool) -> ReturnType {
        ReturnType {
            typ: None,
            is_set_of,
        }
    }
}

impl From<ParamType> for ReturnType {
    fn from(typ: ParamType) -> ReturnType {
        ReturnType::scalar(typ)
    }
}

impl From<ScalarBaseType> for ReturnType {
    fn from(s: ScalarBaseType) -> ReturnType {
        ParamType::from(s).into()
    }
}

impl From<ScalarType> for ReturnType {
    fn from(s: ScalarType) -> ReturnType {
        ParamType::Plain(s).into()
    }
}

#[derive(Clone, Debug)]
/// Tracks candidate implementations.
pub struct Candidate<'a, R> {
    /// The implementation under consideration.
    fimpl: &'a FuncImpl<R>,
    exact_matches: usize,
    preferred_types: usize,
    near_matches: usize,
}

/// Selects the best implementation given the provided `args` using a
/// process similar to [PostgreSQL's parser][pgparser], and returns the
/// `ScalarExpr` to invoke that function.
///
/// Inline comments prefixed with number are taken from the "Function Type
/// Resolution" section of the aforelinked page.
///
/// # Errors
/// - When the provided arguments are not valid for any implementation, e.g.
///   cannot be converted to the appropriate types.
/// - When all implementations are equally valid.
///
/// [pgparser]: https://www.postgresql.org/docs/current/typeconv-oper.html
pub fn select_impl<R>(
    ecx: &ExprContext,
    spec: FuncSpec,
    impls: &[FuncImpl<R>],
    args: Vec<CoercibleScalarExpr>,
    order_by: Vec<ColumnOrder>,
) -> Result<R, PlanError>
where
    R: fmt::Debug,
{
    let name = spec.to_string();
    let ecx = &ecx.with_name(&name);
    let mut types: Vec<_> = args.iter().map(|e| ecx.scalar_type(e)).collect();

    // PostgreSQL force coerces all record types before function selection. We
    // may want to do something smarter in the future (e.g., a function that
    // accepts multiple `RecordAny` parameters should perhaps coerce to the
    // result of calling `guess_best_common_type` on all those parameters), but
    // for now we just directly match PostgreSQL's behavior.
    for ty in &mut types {
        ty.force_coerced_if_record();
    }

    // 4.a. Discard candidate functions for which the input types do not
    // match and cannot be converted (using an implicit conversion) to
    // match. unknown literals are assumed to be convertible to anything for
    // this purpose.
    let impls: Vec<_> = impls
        .iter()
        .filter(|i| i.params.matches_argtypes(ecx, &types))
        .collect();

    let f = find_match(ecx, &types, impls).map_err(|candidates| {
        let arg_types: Vec<_> = types
            .into_iter()
            .map(|ty| match ty {
                // This will be used in error msgs, therefore we call with `postgres_compat` false.
                CoercibleScalarType::Coerced(ty) => ecx.humanize_scalar_type(&ty, false),
                CoercibleScalarType::Record(_) => "record".to_string(),
                CoercibleScalarType::Uncoerced => "unknown".to_string(),
            })
            .collect();

        if candidates == 0 {
            match spec {
                FuncSpec::Func(name) => PlanError::UnknownFunction {
                    name: ecx
                        .qcx
                        .scx
                        .humanize_resolved_name(name)
                        .expect("resolved to object")
                        .to_string(),
                    arg_types,
                },
                FuncSpec::Op(name) => PlanError::UnknownOperator {
                    name: name.to_string(),
                    arg_types,
                },
            }
        } else {
            match spec {
                FuncSpec::Func(name) => PlanError::IndistinctFunction {
                    name: ecx
                        .qcx
                        .scx
                        .humanize_resolved_name(name)
                        .expect("resolved to object")
                        .to_string(),
                    arg_types,
                },
                FuncSpec::Op(name) => PlanError::IndistinctOperator {
                    name: name.to_string(),
                    arg_types,
                },
            }
        }
    })?;

    (f.op.0)(ecx, args, &f.params, order_by)
}

/// Finds an exact match based on the arguments, or, if no exact match, finds
/// the best match available. Patterned after [PostgreSQL's type conversion
/// matching algorithm][pgparser].
///
/// [pgparser]: https://www.postgresql.org/docs/current/typeconv-func.html
fn find_match<'a, R: std::fmt::Debug>(
    ecx: &ExprContext,
    types: &[CoercibleScalarType],
    impls: Vec<&'a FuncImpl<R>>,
) -> Result<&'a FuncImpl<R>, usize> {
    let all_types_known = types.iter().all(|t| t.is_coerced());

    // Check for exact match.
    if all_types_known {
        let known_types: Vec<_> = types.iter().filter_map(|t| t.as_coerced()).collect();
        let matching_impls: Vec<&FuncImpl<_>> = impls
            .iter()
            .filter(|i| i.params.exact_match(&known_types))
            .cloned()
            .collect();

        if matching_impls.len() == 1 {
            return Ok(matching_impls[0]);
        }
    }

    // No exact match. Apply PostgreSQL's best match algorithm. Generate
    // candidates by assessing their compatibility with each implementation's
    // parameters.
    let mut candidates: Vec<Candidate<_>> = Vec::new();
    macro_rules! maybe_get_last_candidate {
        () => {
            if candidates.len() == 1 {
                return Ok(&candidates[0].fimpl);
            }
        };
    }
    let mut max_exact_matches = 0;

    for fimpl in impls {
        let mut exact_matches = 0;
        let mut preferred_types = 0;
        let mut near_matches = 0;

        for (i, arg_type) in types.iter().enumerate() {
            let param_type = &fimpl.params[i];

            match arg_type {
                CoercibleScalarType::Coerced(arg_type) => {
                    if param_type == arg_type {
                        exact_matches += 1;
                    }
                    if param_type.is_preferred_by(arg_type) {
                        preferred_types += 1;
                    }
                    if param_type.is_near_match(arg_type) {
                        near_matches += 1;
                    }
                }
                CoercibleScalarType::Record(_) | CoercibleScalarType::Uncoerced => {
                    if param_type.prefers_self() {
                        preferred_types += 1;
                    }
                }
            }
        }

        // 4.a. Discard candidate functions for which the input types do not
        // match and cannot be converted (using an implicit conversion) to
        // match. unknown literals are assumed to be convertible to anything for
        // this purpose.
        max_exact_matches = std::cmp::max(max_exact_matches, exact_matches);
        candidates.push(Candidate {
            fimpl,
            exact_matches,
            preferred_types,
            near_matches,
        });
    }

    if candidates.is_empty() {
        return Err(0);
    }

    maybe_get_last_candidate!();

    // 4.c. Run through all candidates and keep those with the most exact
    // matches on input types. Keep all candidates if none have exact matches.
    candidates.retain(|c| c.exact_matches >= max_exact_matches);

    maybe_get_last_candidate!();

    // 4.c.i. (MZ extension) Run through all candidates and keep those with the
    // most 'near' matches on input types. Keep all candidates if none have near
    // matches. If only one candidate remains, use it; else continue to the next
    // step.
    let mut max_near_matches = 0;
    for c in &candidates {
        max_near_matches = std::cmp::max(max_near_matches, c.near_matches);
    }
    candidates.retain(|c| c.near_matches >= max_near_matches);

    // 4.d. Run through all candidates and keep those that accept preferred
    // types (of the input data type's type category) at the most positions
    // where type conversion will be required.
    let mut max_preferred_types = 0;
    for c in &candidates {
        max_preferred_types = std::cmp::max(max_preferred_types, c.preferred_types);
    }
    candidates.retain(|c| c.preferred_types >= max_preferred_types);

    maybe_get_last_candidate!();

    if all_types_known {
        return Err(candidates.len());
    }

    let mut found_known = false;
    let mut types_match = true;
    let mut common_type: Option<ScalarType> = None;

    for (i, arg_type) in types.iter().enumerate() {
        let mut selected_category: Option<TypeCategory> = None;
        let mut categories_match = true;

        match arg_type {
            // 4.e. If any input arguments are unknown, check the type
            // categories accepted at those argument positions by the remaining
            // candidates.
            CoercibleScalarType::Uncoerced | CoercibleScalarType::Record(_) => {
                for c in candidates.iter() {
                    let this_category = TypeCategory::from_param(&c.fimpl.params[i]);
                    // 4.e. cont: Select the string category if any candidate
                    // accepts that category. (This bias towards string is
                    // appropriate since an unknown-type literal looks like a
                    // string.)
                    if this_category == TypeCategory::String {
                        selected_category = Some(TypeCategory::String);
                        break;
                    }
                    match selected_category {
                        Some(ref mut selected_category) => {
                            // 4.e. cont: [...otherwise,] if all the remaining candidates
                            // accept the same type category, select that category.
                            categories_match =
                                selected_category == &this_category && categories_match;
                        }
                        None => selected_category = Some(this_category.clone()),
                    }
                }

                // 4.e. cont: Otherwise fail because the correct choice cannot
                // be deduced without more clues.
                // (ed: this doesn't mean fail entirely, simply moving onto 4.f)
                if selected_category != Some(TypeCategory::String) && !categories_match {
                    break;
                }

                // 4.e. cont: Now discard candidates that do not accept the
                // selected type category. Furthermore, if any candidate accepts
                // a preferred type in that category, discard candidates that
                // accept non-preferred types for that argument.
                let selected_category = selected_category.unwrap();

                let preferred_type = selected_category.preferred_type();
                let mut found_preferred_type_candidate = false;
                candidates.retain(|c| {
                    if let Some(typ) = &preferred_type {
                        found_preferred_type_candidate = c.fimpl.params[i].accepts_type(ecx, typ)
                            || found_preferred_type_candidate;
                    }
                    selected_category == TypeCategory::from_param(&c.fimpl.params[i])
                });

                if found_preferred_type_candidate {
                    let preferred_type = preferred_type.unwrap();
                    candidates.retain(|c| c.fimpl.params[i].accepts_type(ecx, &preferred_type));
                }
            }
            CoercibleScalarType::Coerced(typ) => {
                found_known = true;
                // Track if all known types are of the same type; use this info
                // in 4.f.
                match common_type {
                    Some(ref common_type) => types_match = common_type == typ && types_match,
                    None => common_type = Some(typ.clone()),
                }
            }
        }
    }

    maybe_get_last_candidate!();

    // 4.f. If there are both unknown and known-type arguments, and all the
    // known-type arguments have the same type, assume that the unknown
    // arguments are also of that type, and check which candidates can accept
    // that type at the unknown-argument positions.
    // (ed: We know unknown argument exists if we're in this part of the code.)
    if found_known && types_match {
        let common_type = common_type.unwrap();
        let common_typed: Vec<_> = types
            .iter()
            .map(|t| match t {
                CoercibleScalarType::Coerced(t) => CoercibleScalarType::Coerced(t.clone()),
                CoercibleScalarType::Uncoerced | CoercibleScalarType::Record(_) => {
                    CoercibleScalarType::Coerced(common_type.clone())
                }
            })
            .collect();

        candidates.retain(|c| c.fimpl.params.matches_argtypes(ecx, &common_typed));

        maybe_get_last_candidate!();
    }

    Err(candidates.len())
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PolymorphicCompatClass {
    /// Represents the older "Any"-style matching of PG polymorphic types, which
    /// constrains all types to be of the same type, i.e. does not attempt to
    /// promote parameters to a best common type.
    Any,
    /// Represent's Postgres' "anycompatible"-type polymorphic resolution.
    ///
    /// > Selection of the common type considers the actual types of
    /// > anycompatible and anycompatiblenonarray inputs, the array element
    /// > types of anycompatiblearray inputs, the range subtypes of
    /// > anycompatiblerange inputs, and the multirange subtypes of
    /// > anycompatiblemultirange inputs. If anycompatiblenonarray is present
    /// > then the common type is required to be a non-array type. Once a common
    /// > type is identified, arguments in anycompatible and
    /// > anycompatiblenonarray positions are automatically cast to that type,
    /// > and arguments in anycompatiblearray positions are automatically cast
    /// > to the array type for that type.
    ///
    /// For details, see
    /// <https://www.postgresql.org/docs/current/extend-type-system.html#EXTEND-TYPES-POLYMORPHIC>
    BestCommonAny,
    /// Represents polymorphic compatibility operations for Materialize LIST
    /// types. This differs from PG's "anycompatible" type resolution, which
    /// focuses on determining a common type, and e.g. using that as
    /// `AnyCompatibleArray`'s element type. Instead, our list compatibility
    /// focuses on finding a common list type, and then casting
    /// `ListElementAnyCompatible` parameters to that list type's elements. This
    /// approach is necessary to let us polymorphically resolve custom list
    /// types without losing their OIDs.
    BestCommonList,
    /// Represents an operation similar to LIST compatibility, but for MAP. This
    /// is distinct from `BestCommonList` in as much as the parameter types that
    /// work with `BestCommonList` are incommensurate with the parameter types
    /// used with `BestCommonMap`.
    BestCommonMap,
    /// Represents type resolution for `ScalarType::Record` types, which e.g.
    /// ignores custom types and type modifications.
    ///
    /// In [PG], this is handled by invocation of the function calls that take
    /// `RecordAny` params, which we want to avoid if at all possible.
    ///
    /// [PG]:
    ///     https://github.com/postgres/postgres/blob/33a377608fc29cdd1f6b63be561eab0aee5c81f0/src/backend/utils/adt/rowtypes.c#L1041
    StructuralEq,
}

impl TryFrom<&ParamType> for PolymorphicCompatClass {
    type Error = ();
    fn try_from(param: &ParamType) -> Result<PolymorphicCompatClass, Self::Error> {
        use ParamType::*;

        Ok(match param {
            AnyElement | ArrayAny | ListAny | MapAny | NonVecAny | RangeAny => {
                PolymorphicCompatClass::Any
            }
            ArrayAnyCompatible | AnyCompatible | RangeAnyCompatible | NonVecAnyCompatible => {
                PolymorphicCompatClass::BestCommonAny
            }
            ListAnyCompatible | ListElementAnyCompatible => PolymorphicCompatClass::BestCommonList,
            MapAnyCompatible => PolymorphicCompatClass::BestCommonMap,
            RecordAny => PolymorphicCompatClass::StructuralEq,
            _ => return Err(()),
        })
    }
}

impl PolymorphicCompatClass {
    fn compatible(&self, ecx: &ExprContext, from: &ScalarType, to: &ScalarType) -> bool {
        use PolymorphicCompatClass::*;
        match self {
            StructuralEq => from.structural_eq(to),
            Any => from.base_eq(to),
            _ => typeconv::can_cast(ecx, CastContext::Implicit, from, to),
        }
    }
}

/// Represents a solution to a set of polymorphic constraints, expressed as the
/// `params` of a function and the user-supplied `args`.
#[derive(Debug)]
pub(crate) struct PolymorphicSolution {
    /// Constrains this solution to a particular form of polymorphic
    /// compatibility.
    compat: Option<PolymorphicCompatClass>,
    seen: Vec<CoercibleScalarType>,
    /// An internal representation of the discovered polymorphic type.
    key: Option<ScalarType>,
}

impl PolymorphicSolution {
    /// Provides a solution to the polymorphic type constraints expressed in
    /// `params` based on the users' input in `args`. Returns `None` if a
    /// solution cannot be found.
    ///
    /// After constructing the `PolymorphicSolution`, access its solution using
    /// [`PolymorphicSolution::target_for_param_type`].
    fn new(
        ecx: &ExprContext,
        args: &[CoercibleScalarType],
        params: &ParamList,
    ) -> Option<PolymorphicSolution> {
        let mut r = PolymorphicSolution {
            compat: None,
            seen: vec![],
            key: None,
        };

        for (i, scalar_type) in args.iter().cloned().enumerate() {
            r.track_seen(&params[i], scalar_type);
        }

        if !r.determine_key(ecx) { None } else { Some(r) }
    }

    /// Determines the desired type of polymorphic compatibility, as well as the
    /// values to determine a polymorphic solution.
    fn track_seen(&mut self, param: &ParamType, seen: CoercibleScalarType) {
        use ParamType::*;

        self.seen.push(match param {
            // These represent the keys of their respective compatibility classes.
            AnyElement | AnyCompatible | ListAnyCompatible |  MapAnyCompatible | NonVecAny | RecordAny => seen,
            MapAny => seen.map_coerced(|array| array.unwrap_map_value_type().clone()),
            ListAny => seen.map_coerced(|array| array.unwrap_list_element_type().clone()),
            ArrayAny | ArrayAnyCompatible => seen.map_coerced(|array| array.unwrap_array_element_type().clone()),
            RangeAny | RangeAnyCompatible => seen.map_coerced(|range| range.unwrap_range_element_type().clone()),
            ListElementAnyCompatible => seen.map_coerced(|el| ScalarType::List {
                custom_id: None,
                element_type: Box::new(el),
            }),
            o => {
                assert!(
                    !o.is_polymorphic(),
                    "polymorphic parameters must track types they encounter to determine polymorphic solution"
                );
                return;
            }
        });

        let compat_class = param
            .try_into()
            .expect("already returned for non-polymorphic params");

        match &self.compat {
            None => self.compat = Some(compat_class),
            Some(c) => {
                assert_eq!(
                    c, &compat_class,
                    "do not know how to correlate polymorphic classes {:?} and {:?}",
                    c, &compat_class,
                )
            }
        };
    }

    /// Attempt to resolve all polymorphic types to a single "key" type. For
    /// `target_for_param_type` to be useful, this must have already been
    /// called.
    fn determine_key(&mut self, ecx: &ExprContext) -> bool {
        self.key = if !self.seen.iter().any(|v| v.is_coerced()) {
            match &self.compat {
                // No encountered param was polymorphic
                None => None,
                // Params were polymorphic, but we never received a known type.
                // This cannot be delegated to `guess_best_common_type`, which
                // will incorrectly guess string, which is incompatible with
                // `BestCommonList`, `BestCommonMap`.
                Some(t) => match t {
                    PolymorphicCompatClass::BestCommonAny => Some(ScalarType::String),
                    PolymorphicCompatClass::BestCommonList => Some(ScalarType::List {
                        custom_id: None,
                        element_type: Box::new(ScalarType::String),
                    }),
                    PolymorphicCompatClass::BestCommonMap => Some(ScalarType::Map {
                        value_type: Box::new(ScalarType::String),
                        custom_id: None,
                    }),
                    // Do not infer type.
                    PolymorphicCompatClass::StructuralEq | PolymorphicCompatClass::Any => None,
                },
            }
        } else {
            // If we saw any polymorphic parameters, we must have determined the
            // compatibility type.
            let compat = self.compat.as_ref().unwrap();

            let r = match compat {
                PolymorphicCompatClass::Any => {
                    let mut s = self
                        .seen
                        .iter()
                        .filter_map(|f| f.as_coerced().cloned())
                        .collect::<Vec<_>>();
                    let (candiate, remaining) =
                        s.split_first().expect("have at least one non-None element");
                    if remaining.iter().all(|r| r.base_eq(candiate)) {
                        s.remove(0)
                    } else {
                        return false;
                    }
                }
                _ => match typeconv::guess_best_common_type(ecx, &self.seen) {
                    Ok(r) => r,
                    Err(_) => return false,
                },
            };

            // Ensure the best common type is compatible.
            for t in self.seen.iter() {
                if let CoercibleScalarType::Coerced(t) = t {
                    if !compat.compatible(ecx, t, &r) {
                        return false;
                    }
                }
            }
            Some(r)
        };

        true
    }

    // Determines the appropriate `ScalarType` for the given `ParamType` based
    // on the polymorphic solution.
    fn target_for_param_type(&self, param: &ParamType) -> Option<ScalarType> {
        use ParamType::*;
        assert_eq!(
            self.compat,
            Some(
                param
                    .try_into()
                    .expect("target_for_param_type only supports polymorphic parameters")
            ),
            "cannot use polymorphic solution for different compatibility classes"
        );

        assert!(
            !matches!(param, RecordAny),
            "RecordAny should not be cast to a target type"
        );

        match param {
            AnyElement | AnyCompatible | ListAnyCompatible | MapAnyCompatible | NonVecAny => {
                self.key.clone()
            }
            ArrayAny | ArrayAnyCompatible => self
                .key
                .as_ref()
                .map(|key| ScalarType::Array(Box::new(key.clone()))),
            ListAny => self.key.as_ref().map(|key| ScalarType::List {
                element_type: Box::new(key.clone()),
                custom_id: None,
            }),
            MapAny => self.key.as_ref().map(|key| ScalarType::Map {
                value_type: Box::new(key.clone()),
                custom_id: None,
            }),
            RangeAny | RangeAnyCompatible => self.key.as_ref().map(|key| ScalarType::Range {
                element_type: Box::new(key.clone()),
            }),
            ListElementAnyCompatible => self
                .key
                .as_ref()
                .map(|key| key.unwrap_list_element_type().clone()),
            _ => unreachable!(
                "cannot use polymorphic solution to resolve target type for param {:?}",
                param,
            ),
        }
    }
}

fn coerce_args_to_types(
    ecx: &ExprContext,
    args: Vec<CoercibleScalarExpr>,
    params: &ParamList,
) -> Result<Vec<HirScalarExpr>, PlanError> {
    use ParamType::*;

    let mut scalar_types: Vec<_> = args.iter().map(|e| ecx.scalar_type(e)).collect();

    // See comment in `select_impl`.
    for ty in &mut scalar_types {
        ty.force_coerced_if_record();
    }

    let polymorphic_solution = PolymorphicSolution::new(ecx, &scalar_types, params)
        .expect("polymorphic solution previously determined to be valid");

    let do_convert =
        |arg: CoercibleScalarExpr, ty: &ScalarType| arg.cast_to(ecx, CastContext::Implicit, ty);

    let mut res_exprs = Vec::with_capacity(args.len());
    for (i, cexpr) in args.into_iter().enumerate() {
        let expr = match &params[i] {
            Any => match cexpr {
                CoercibleScalarExpr::Parameter(n) => {
                    sql_bail!("could not determine data type of parameter ${}", n)
                }
                _ => cexpr.type_as_any(ecx)?,
            },
            RecordAny => match cexpr {
                CoercibleScalarExpr::LiteralString(_) => {
                    sql_bail!("input of anonymous composite types is not implemented");
                }
                // By passing the creation of the polymorphic solution, we've
                // already ensured that all of the record types are
                // intrinsically well-typed enough to move onto the next step.
                _ => cexpr.type_as_any(ecx)?,
            },
            Plain(ty) => do_convert(cexpr, ty)?,
            Internal => return Err(PlanError::InternalFunctionCall),
            p => {
                let target = polymorphic_solution
                    .target_for_param_type(p)
                    .ok_or_else(|| {
                        // n.b. This errors here, rather than during building
                        // the polymorphic solution, to make the error clearer.
                        // If we errored while constructing the polymorphic
                        // solution, an implementation would get discarded even
                        // if it were the only one, and it would appear as if a
                        // compatible solution did not exist. Instead, the
                        // problem is simply that we couldn't resolve the
                        // polymorphic type.
                        PlanError::UnsolvablePolymorphicFunctionInput
                    })?;
                do_convert(cexpr, &target)?
            }
        };
        res_exprs.push(expr);
    }

    Ok(res_exprs)
}

/// Provides shorthand for converting `Vec<ScalarType>` into `Vec<ParamType>`.
macro_rules! params {
    ([$($p:expr),*], $v:ident...) => { ParamList::Variadic { leading: vec![$($p.into(),)*], trailing: $v.into() } };
    ($v:ident...) => { ParamList::Variadic { leading: vec![], trailing: $v.into() } };
    ($($p:expr),*) => { ParamList::Exact(vec![$($p.into(),)*]) };
}

macro_rules! impl_def {
    // Return type explicitly specified. This must be the case in situations
    // such as:
    // - Polymorphic functions: We have no way of understanding if the input
    //   type affects the return type, so you must tell us what the return type
    //   is.
    // - Explicitly defined Operations whose returned expression does not
    //   appropriately correlate to the function itself, e.g. returning a
    //   UnaryFunc from a FuncImpl that takes two parameters.
    // - Unimplemented/catalog-only functions
    ($params:expr, $op:expr, $return_type:expr, $oid:expr) => {{
        FuncImpl {
            oid: $oid,
            params: $params.into(),
            op: $op.into(),
            return_type: $return_type.into(),
        }
    }};
}

/// Constructs builtin function map.
macro_rules! builtins {
    {
        $(
            $name:expr => $ty:ident {
                $($params:expr => $op:expr => $return_type:expr, $oid:expr;)+
            }
        ),+
    } => {{

        let mut builtins = BTreeMap::new();
        $(
            let impls = vec![$(impl_def!($params, $op, $return_type, $oid)),+];
            let func = Func::$ty(impls);
            let expect_set_return = matches!(&func, Func::Table(_));
            for imp in func.func_impls() {
                assert_eq!(imp.return_is_set, expect_set_return, "wrong set return value for func with oid {}", imp.oid);
            }
            let old = builtins.insert($name, func);
            mz_ore::assert_none!(old, "duplicate entry in builtins list");
        )+
        builtins
    }};
}

#[derive(Debug)]
pub struct TableFuncPlan {
    pub expr: HirRelationExpr,
    pub column_names: Vec<ColumnName>,
}

#[derive(Debug)]
pub enum Func {
    Scalar(Vec<FuncImpl<HirScalarExpr>>),
    Aggregate(Vec<FuncImpl<(HirScalarExpr, AggregateFunc)>>),
    Table(Vec<FuncImpl<TableFuncPlan>>),
    ScalarWindow(Vec<FuncImpl<ScalarWindowFunc>>),
    ValueWindow(Vec<FuncImpl<(HirScalarExpr, ValueWindowFunc)>>),
}

impl Func {
    pub fn func_impls(&self) -> Vec<FuncImplCatalogDetails> {
        match self {
            Func::Scalar(impls) => impls.iter().map(|f| f.details()).collect::<Vec<_>>(),
            Func::Aggregate(impls) => impls.iter().map(|f| f.details()).collect::<Vec<_>>(),
            Func::Table(impls) => impls.iter().map(|f| f.details()).collect::<Vec<_>>(),
            Func::ScalarWindow(impls) => impls.iter().map(|f| f.details()).collect::<Vec<_>>(),
            Func::ValueWindow(impls) => impls.iter().map(|f| f.details()).collect::<Vec<_>>(),
        }
    }

    pub fn class(&self) -> &str {
        match self {
            Func::Scalar(..) => "scalar",
            Func::Aggregate(..) => "aggregate",
            Func::Table(..) => "table",
            Func::ScalarWindow(..) => "window",
            Func::ValueWindow(..) => "window",
        }
    }
}

/// Functions using this macro should be transformed/planned away before
/// reaching function selection code, but still need to be present in the
/// catalog during planning.
macro_rules! catalog_name_only {
    ($name:expr) => {
        panic!(
            "{} should be planned away before reaching function selection",
            $name
        )
    };
}

/// Generates an (OID, OID, TEXT) SQL implementation for has_X_privilege style functions.
macro_rules! privilege_fn {
    ( $fn_name:expr, $catalog_tbl:expr ) => {
        {
            let fn_name = $fn_name;
            let catalog_tbl = $catalog_tbl;
            let public_role = RoleId::Public;
            format!(
                "
                    CASE
                    -- We need to validate the privileges to return a proper error before anything
                    -- else.
                    WHEN NOT mz_internal.mz_validate_privileges($3)
                    OR $1 IS NULL
                    OR $2 IS NULL
                    OR $3 IS NULL
                    OR $1 NOT IN (SELECT oid FROM mz_catalog.mz_roles)
                    OR $2 NOT IN (SELECT oid FROM {catalog_tbl})
                    THEN NULL
                    ELSE COALESCE(
                        (
                            SELECT
                                bool_or(
                                    mz_internal.mz_acl_item_contains_privilege(privilege, $3)
                                )
                                    AS {fn_name}
                            FROM
                                (
                                    SELECT
                                        unnest(privileges)
                                    FROM
                                        {catalog_tbl}
                                    WHERE
                                        {catalog_tbl}.oid = $2
                                )
                                    AS user_privs (privilege)
                                LEFT JOIN mz_catalog.mz_roles ON
                                        mz_internal.mz_aclitem_grantee(privilege) = mz_roles.id
                            WHERE
                                mz_internal.mz_aclitem_grantee(privilege) = '{public_role}' OR pg_has_role($1, mz_roles.oid, 'USAGE')
                        ),
                        false
                    )
                    END
                ",
            )
        }
    };
}

/// Correlates a built-in function name to its implementations.
pub static PG_CATALOG_BUILTINS: LazyLock<BTreeMap<&'static str, Func>> = LazyLock::new(|| {
    use ParamType::*;
    use ScalarBaseType::*;
    let mut builtins = builtins! {
        // Literal OIDs collected from PG 13 using a version of this query
        // ```sql
        // SELECT oid, proname, proargtypes::regtype[]
        // FROM pg_proc
        // WHERE proname IN (
        //      'ascii', 'array_upper', 'jsonb_build_object'
        // );
        // ```
        // Values are also available through
        // https://github.com/postgres/postgres/blob/master/src/include/catalog/pg_proc.dat

        // Scalars.
        "abs" => Scalar {
            params!(Int16) => UnaryFunc::AbsInt16(func::AbsInt16) => Int16, 1398;
            params!(Int32) => UnaryFunc::AbsInt32(func::AbsInt32) => Int32, 1397;
            params!(Int64) => UnaryFunc::AbsInt64(func::AbsInt64) => Int64, 1396;
            params!(Numeric) => UnaryFunc::AbsNumeric(func::AbsNumeric) => Numeric, 1705;
            params!(Float32) => UnaryFunc::AbsFloat32(func::AbsFloat32) => Float32, 1394;
            params!(Float64) => UnaryFunc::AbsFloat64(func::AbsFloat64) => Float64, 1395;
        },
        "aclexplode" => Table {
            params!(ScalarType::Array(Box::new(ScalarType::AclItem))) =>  Operation::unary(move |_ecx, aclitems| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::AclExplode,
                        exprs: vec![aclitems],
                    },
                    column_names: vec!["grantor".into(), "grantee".into(), "privilege_type".into(), "is_grantable".into()],
                })
            }) => ReturnType::set_of(RecordAny), 1689;
        },
        "array_cat" => Scalar {
            params!(ArrayAnyCompatible, ArrayAnyCompatible) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs.call_binary(rhs, BinaryFunc::ArrayArrayConcat))
            }) => ArrayAnyCompatible, 383;
        },
        "array_fill" => Scalar {
            params!(AnyElement, ScalarType::Array(Box::new(ScalarType::Int32))) => Operation::binary(|ecx, elem, dims| {
                let elem_type = ecx.scalar_type(&elem);

                let elem_type = match elem_type.array_of_self_elem_type() {
                    Ok(elem_type) => elem_type,
                    Err(elem_type) => bail_unsupported!(
                        // This will be used in error msgs, therefore we call with `postgres_compat` false.
                        format!("array_fill on {}", ecx.humanize_scalar_type(&elem_type, false))
                    ),
                };

                Ok(HirScalarExpr::call_variadic(VariadicFunc::ArrayFill { elem_type }, vec![elem, dims]))
            }) => ArrayAny, 1193;
            params!(
                AnyElement,
                ScalarType::Array(Box::new(ScalarType::Int32)),
                ScalarType::Array(Box::new(ScalarType::Int32))
            ) => Operation::variadic(|ecx, exprs| {
                let elem_type = ecx.scalar_type(&exprs[0]);

                let elem_type = match elem_type.array_of_self_elem_type() {
                    Ok(elem_type) => elem_type,
                    Err(elem_type) => bail_unsupported!(
                        format!("array_fill on {}", ecx.humanize_scalar_type(&elem_type, false))
                    ),
                };

                Ok(HirScalarExpr::call_variadic(VariadicFunc::ArrayFill { elem_type }, exprs))
            }) => ArrayAny, 1286;
        },
        "array_length" => Scalar {
            params![ArrayAny, Int64] => BinaryFunc::ArrayLength => Int32, 2176;
        },
        "array_lower" => Scalar {
            params!(ArrayAny, Int64) => BinaryFunc::ArrayLower => Int32, 2091;
        },
        "array_position" => Scalar {
            params!(ArrayAnyCompatible, AnyCompatible) => VariadicFunc::ArrayPosition => Int32, 3277;
            params!(ArrayAnyCompatible, AnyCompatible, Int32) => VariadicFunc::ArrayPosition => Int32, 3278;
        },
        "array_remove" => Scalar {
            params!(ArrayAnyCompatible, AnyCompatible) => BinaryFunc::ArrayRemove => ArrayAnyCompatible, 3167;
        },
        "array_to_string" => Scalar {
            params!(ArrayAny, String) => Operation::variadic(array_to_string) => String, 395;
            params!(ArrayAny, String, String) => Operation::variadic(array_to_string) => String, 384;
        },
        "array_upper" => Scalar {
            params!(ArrayAny, Int64) => BinaryFunc::ArrayUpper => Int32, 2092;
        },
        "ascii" => Scalar {
            params!(String) => UnaryFunc::Ascii(func::Ascii) => Int32, 1620;
        },
        "avg" => Scalar {
            params!(Int64) => Operation::nullary(|_ecx| catalog_name_only!("avg")) => Numeric, 2100;
            params!(Int32) => Operation::nullary(|_ecx| catalog_name_only!("avg")) => Numeric, 2101;
            params!(Int16) => Operation::nullary(|_ecx| catalog_name_only!("avg")) => Numeric, 2102;
            params!(UInt64) => Operation::nullary(|_ecx| catalog_name_only!("avg")) => Numeric, oid::FUNC_AVG_UINT64_OID;
            params!(UInt32) => Operation::nullary(|_ecx| catalog_name_only!("avg")) => Numeric, oid::FUNC_AVG_UINT32_OID;
            params!(UInt16) => Operation::nullary(|_ecx| catalog_name_only!("avg")) => Numeric, oid::FUNC_AVG_UINT16_OID;
            params!(Float32) => Operation::nullary(|_ecx| catalog_name_only!("avg")) => Float64, 2104;
            params!(Float64) => Operation::nullary(|_ecx| catalog_name_only!("avg")) => Float64, 2105;
            params!(Interval) => Operation::nullary(|_ecx| catalog_name_only!("avg")) => Interval, 2106;
        },
        "bit_count" => Scalar {
            params!(Bytes) => UnaryFunc::BitCountBytes(func::BitCountBytes) => Int64, 6163;
        },
        "bit_length" => Scalar {
            params!(Bytes) => UnaryFunc::BitLengthBytes(func::BitLengthBytes) => Int32, 1810;
            params!(String) => UnaryFunc::BitLengthString(func::BitLengthString) => Int32, 1811;
        },
        "btrim" => Scalar {
            params!(String) => UnaryFunc::TrimWhitespace(func::TrimWhitespace) => String, 885;
            params!(String, String) => BinaryFunc::Trim => String, 884;
        },
        "cbrt" => Scalar {
            params!(Float64) => UnaryFunc::CbrtFloat64(func::CbrtFloat64) => Float64, 1345;
        },
        "ceil" => Scalar {
            params!(Float32) => UnaryFunc::CeilFloat32(func::CeilFloat32) => Float32, oid::FUNC_CEIL_F32_OID;
            params!(Float64) => UnaryFunc::CeilFloat64(func::CeilFloat64) => Float64, 2308;
            params!(Numeric) => UnaryFunc::CeilNumeric(func::CeilNumeric) => Numeric, 1711;
        },
        "ceiling" => Scalar {
            params!(Float32) => UnaryFunc::CeilFloat32(func::CeilFloat32) => Float32, oid::FUNC_CEILING_F32_OID;
            params!(Float64) => UnaryFunc::CeilFloat64(func::CeilFloat64) => Float64, 2320;
            params!(Numeric) => UnaryFunc::CeilNumeric(func::CeilNumeric) => Numeric, 2167;
        },
        "char_length" => Scalar {
            params!(String) => UnaryFunc::CharLength(func::CharLength) => Int32, 1381;
        },
        // SQL exactly matches PostgreSQL's implementation.
        "col_description" => Scalar {
            params!(Oid, Int32) => sql_impl_func(
                "(SELECT description
                    FROM pg_description
                    WHERE objoid = $1 AND classoid = 'pg_class'::regclass AND objsubid = $2)"
                ) => String, 1216;
        },
        "concat" => Scalar {
            params!(Any...) => Operation::variadic(|ecx, cexprs| {
                if cexprs.is_empty() {
                    sql_bail!("No function matches the given name and argument types. \
                    You might need to add explicit type casts.")
                }
                let mut exprs = vec![];
                for expr in cexprs {
                    exprs.push(match ecx.scalar_type(&expr) {
                        // concat uses nonstandard bool -> string casts
                        // to match historical baggage in PostgreSQL.
                        ScalarType::Bool => expr.call_unary(UnaryFunc::CastBoolToStringNonstandard(func::CastBoolToStringNonstandard)),
                        // TODO(see <materialize#7572>): remove call to PadChar
                        ScalarType::Char { length } => expr.call_unary(UnaryFunc::PadChar(func::PadChar { length })),
                        _ => typeconv::to_string(ecx, expr)
                    });
                }
                Ok(HirScalarExpr::call_variadic(VariadicFunc::Concat, exprs))
            }) => String, 3058;
        },
        "concat_ws" => Scalar {
            params!([String], Any...) => Operation::variadic(|ecx, cexprs| {
                if cexprs.len() < 2 {
                    sql_bail!("No function matches the given name and argument types. \
                    You might need to add explicit type casts.")
                }
                let mut exprs = vec![];
                for expr in cexprs {
                    exprs.push(match ecx.scalar_type(&expr) {
                        // concat uses nonstandard bool -> string casts
                        // to match historical baggage in PostgreSQL.
                        ScalarType::Bool => expr.call_unary(UnaryFunc::CastBoolToStringNonstandard(func::CastBoolToStringNonstandard)),
                        // TODO(see <materialize#7572>): remove call to PadChar
                        ScalarType::Char { length } => expr.call_unary(UnaryFunc::PadChar(func::PadChar { length })),
                        _ => typeconv::to_string(ecx, expr)
                    });
                }
                Ok(HirScalarExpr::call_variadic(VariadicFunc::ConcatWs, exprs))
            }) => String, 3059;
        },
        "convert_from" => Scalar {
            params!(Bytes, String) => BinaryFunc::ConvertFrom => String, 1714;
        },
        "cos" => Scalar {
            params!(Float64) => UnaryFunc::Cos(func::Cos) => Float64, 1605;
        },
        "acos" => Scalar {
            params!(Float64) => UnaryFunc::Acos(func::Acos) => Float64, 1601;
        },
        "cosh" => Scalar {
            params!(Float64) => UnaryFunc::Cosh(func::Cosh) => Float64, 2463;
        },
        "acosh" => Scalar {
            params!(Float64) => UnaryFunc::Acosh(func::Acosh) => Float64, 2466;
        },
        "cot" => Scalar {
            params!(Float64) => UnaryFunc::Cot(func::Cot) => Float64, 1607;
        },
        "current_schema" => Scalar {
            // TODO: this should be `name`. This is tricky in Materialize
            // because `name` truncates to 63 characters but Materialize does
            // not have a limit on identifier length.
            params!() => UnmaterializableFunc::CurrentSchema => String, 1402;
        },
        "current_schemas" => Scalar {
            params!(Bool) => Operation::unary(|_ecx, e| {
                Ok(HirScalarExpr::if_then_else(
                     e,
                     HirScalarExpr::call_unmaterializable(UnmaterializableFunc::CurrentSchemasWithSystem),
                     HirScalarExpr::call_unmaterializable(UnmaterializableFunc::CurrentSchemasWithoutSystem),
                ))
                // TODO: this should be `name[]`. This is tricky in Materialize
                // because `name` truncates to 63 characters but Materialize
                // does not have a limit on identifier length.
            }) => ScalarType::Array(Box::new(ScalarType::String)), 1403;
        },
        "current_database" => Scalar {
            params!() => UnmaterializableFunc::CurrentDatabase => String, 861;
        },
        "current_catalog" => Scalar {
            params!() => UnmaterializableFunc::CurrentDatabase => String, oid::FUNC_CURRENT_CATALOG;
        },
        "current_setting" => Scalar {
            params!(String) => Operation::unary(|_ecx, name| {
                current_settings(name, HirScalarExpr::literal_false())
            }) => ScalarType::String, 2077;
            params!(String, Bool) => Operation::binary(|_ecx, name, missing_ok| {
                current_settings(name, missing_ok)
            }) => ScalarType::String, 3294;
        },
        "current_timestamp" => Scalar {
            params!() => UnmaterializableFunc::CurrentTimestamp => TimestampTz, oid::FUNC_CURRENT_TIMESTAMP_OID;
        },
        "current_user" => Scalar {
            params!() => UnmaterializableFunc::CurrentUser => String, 745;
        },
        "current_role" => Scalar {
            params!() => UnmaterializableFunc::CurrentUser => String, oid::FUNC_CURRENT_ROLE;
        },
        "user" => Scalar {
            params!() => UnmaterializableFunc::CurrentUser => String, oid::FUNC_USER;
        },
        "session_user" => Scalar {
            params!() => UnmaterializableFunc::SessionUser => String, 746;
        },
        "chr" => Scalar {
            params!(Int32) => UnaryFunc::Chr(func::Chr) => String, 1621;
        },
        "date" => Scalar {
            params!(String) => UnaryFunc::CastStringToDate(func::CastStringToDate) => Date, oid::FUNC_DATE_FROM_TEXT;
            params!(Timestamp) => UnaryFunc::CastTimestampToDate(func::CastTimestampToDate) => Date, 2029;
            params!(TimestampTz) => UnaryFunc::CastTimestampTzToDate(func::CastTimestampTzToDate) => Date, 1178;
        },
        "date_bin" => Scalar {
            params!(Interval, Timestamp) => Operation::binary(|ecx, stride, source| {
                ecx.require_feature_flag(&vars::ENABLE_BINARY_DATE_BIN)?;
                Ok(stride.call_binary(source, BinaryFunc::DateBinTimestamp))
            }) => Timestamp, oid::FUNC_MZ_DATE_BIN_UNIX_EPOCH_TS_OID;
            params!(Interval, TimestampTz) => Operation::binary(|ecx, stride, source| {
                ecx.require_feature_flag(&vars::ENABLE_BINARY_DATE_BIN)?;
                Ok(stride.call_binary(source, BinaryFunc::DateBinTimestampTz))
            }) => TimestampTz, oid::FUNC_MZ_DATE_BIN_UNIX_EPOCH_TSTZ_OID;
            params!(Interval, Timestamp, Timestamp) => VariadicFunc::DateBinTimestamp => Timestamp, 6177;
            params!(Interval, TimestampTz, TimestampTz) => VariadicFunc::DateBinTimestampTz => TimestampTz, 6178;
        },
        "extract" => Scalar {
            params!(String, Interval) => BinaryFunc::ExtractInterval => Numeric, 6204;
            params!(String, Time) => BinaryFunc::ExtractTime => Numeric, 6200;
            params!(String, Timestamp) => BinaryFunc::ExtractTimestamp => Numeric, 6202;
            params!(String, TimestampTz) => BinaryFunc::ExtractTimestampTz => Numeric, 6203;
            params!(String, Date) => BinaryFunc::ExtractDate => Numeric, 6199;
        },
        "date_part" => Scalar {
            params!(String, Interval) => BinaryFunc::DatePartInterval => Float64, 1172;
            params!(String, Time) => BinaryFunc::DatePartTime => Float64, 1385;
            params!(String, Timestamp) => BinaryFunc::DatePartTimestamp => Float64, 2021;
            params!(String, TimestampTz) => BinaryFunc::DatePartTimestampTz => Float64, 1171;
        },
        "date_trunc" => Scalar {
            params!(String, Timestamp) => BinaryFunc::DateTruncTimestamp => Timestamp, 2020;
            params!(String, TimestampTz) => BinaryFunc::DateTruncTimestampTz => TimestampTz, 1217;
            params!(String, Interval) => BinaryFunc::DateTruncInterval => Interval, 1218;
        },
        "daterange" => Scalar {
            params!(Date, Date) => Operation::variadic(|_ecx, mut exprs| {
                exprs.push(HirScalarExpr::literal(Datum::String("[)"), ScalarType::String));
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::Date },
                    exprs))
            }) => ScalarType::Range { element_type: Box::new(ScalarType::Date)}, 3941;
            params!(Date, Date, String) => Operation::variadic(|_ecx, exprs| {
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::Date },
                    exprs))
            }) => ScalarType::Range { element_type: Box::new(ScalarType::Date)}, 3942;
        },
        "degrees" => Scalar {
            params!(Float64) => UnaryFunc::Degrees(func::Degrees) => Float64, 1608;
        },
        "digest" => Scalar {
            params!(String, String) => BinaryFunc::DigestString => Bytes, oid::FUNC_PG_DIGEST_STRING;
            params!(Bytes, String) => BinaryFunc::DigestBytes => Bytes, oid::FUNC_PG_DIGEST_BYTES;
        },
        "exp" => Scalar {
            params!(Float64) => UnaryFunc::Exp(func::Exp) => Float64, 1347;
            params!(Numeric) => UnaryFunc::ExpNumeric(func::ExpNumeric) => Numeric, 1732;
        },
        "floor" => Scalar {
            params!(Float32) => UnaryFunc::FloorFloat32(func::FloorFloat32) => Float32, oid::FUNC_FLOOR_F32_OID;
            params!(Float64) => UnaryFunc::FloorFloat64(func::FloorFloat64) => Float64, 2309;
            params!(Numeric) => UnaryFunc::FloorNumeric(func::FloorNumeric) => Numeric, 1712;
        },
        "format_type" => Scalar {
            params!(Oid, Int32) => sql_impl_func(
                "CASE
                        WHEN $1 IS NULL THEN NULL
                        -- timestamp and timestamptz have the typmod in
                        -- a nonstandard location that requires special
                        -- handling.
                        WHEN $1 = 1114 AND $2 >= 0 THEN 'timestamp(' || $2 || ') without time zone'
                        WHEN $1 = 1184 AND $2 >= 0 THEN 'timestamp(' || $2 || ') with time zone'
                        ELSE coalesce((SELECT pg_catalog.concat(coalesce(mz_internal.mz_type_name($1), name), mz_internal.mz_render_typmod($1, $2)) FROM mz_catalog.mz_types WHERE oid = $1), '???')
                    END"
            ) => String, 1081;
        },
        "get_bit" => Scalar {
            params!(Bytes, Int32) => BinaryFunc::GetBit => Int32, 723;
        },
        "get_byte" => Scalar {
            params!(Bytes, Int32) => BinaryFunc::GetByte => Int32, 721;
        },
        "pg_get_ruledef" => Scalar {
            params!(Oid) => sql_impl_func("NULL::pg_catalog.text") => String, 1573;
            params!(Oid, Bool) => sql_impl_func("NULL::pg_catalog.text") => String, 2504;
        },
        "has_schema_privilege" => Scalar {
            params!(String, String, String) => sql_impl_func("has_schema_privilege(mz_internal.mz_role_oid($1), mz_internal.mz_schema_oid($2), $3)") => Bool, 2268;
            params!(String, Oid, String) => sql_impl_func("has_schema_privilege(mz_internal.mz_role_oid($1), $2, $3)") => Bool, 2269;
            params!(Oid, String, String) => sql_impl_func("has_schema_privilege($1, mz_internal.mz_schema_oid($2), $3)") => Bool, 2270;
            params!(Oid, Oid, String) => sql_impl_func(&privilege_fn!("has_schema_privilege", "mz_schemas")) => Bool, 2271;
            params!(String, String) => sql_impl_func("has_schema_privilege(current_user, $1, $2)") => Bool, 2272;
            params!(Oid, String) => sql_impl_func("has_schema_privilege(current_user, $1, $2)") => Bool, 2273;
        },
        "has_database_privilege" => Scalar {
            params!(String, String, String) => sql_impl_func("has_database_privilege(mz_internal.mz_role_oid($1), mz_internal.mz_database_oid($2), $3)") => Bool, 2250;
            params!(String, Oid, String) => sql_impl_func("has_database_privilege(mz_internal.mz_role_oid($1), $2, $3)") => Bool, 2251;
            params!(Oid, String, String) => sql_impl_func("has_database_privilege($1, mz_internal.mz_database_oid($2), $3)") => Bool, 2252;
            params!(Oid, Oid, String) => sql_impl_func(&privilege_fn!("has_database_privilege", "mz_databases")) => Bool, 2253;
            params!(String, String) => sql_impl_func("has_database_privilege(current_user, $1, $2)") => Bool, 2254;
            params!(Oid, String) => sql_impl_func("has_database_privilege(current_user, $1, $2)") => Bool, 2255;
        },
        "has_table_privilege" => Scalar {
            params!(String, String, String) => sql_impl_func("has_table_privilege(mz_internal.mz_role_oid($1), $2::regclass::oid, $3)") => Bool, 1922;
            params!(String, Oid, String) => sql_impl_func("has_table_privilege(mz_internal.mz_role_oid($1), $2, $3)") => Bool, 1923;
            params!(Oid, String, String) => sql_impl_func("has_table_privilege($1, $2::regclass::oid, $3)") => Bool, 1924;
            params!(Oid, Oid, String) => sql_impl_func(&privilege_fn!("has_table_privilege", "mz_relations")) => Bool, 1925;
            params!(String, String) => sql_impl_func("has_table_privilege(current_user, $1, $2)") => Bool, 1926;
            params!(Oid, String) => sql_impl_func("has_table_privilege(current_user, $1, $2)") => Bool, 1927;
        },
        "hmac" => Scalar {
            params!(String, String, String) => VariadicFunc::HmacString => Bytes, oid::FUNC_PG_HMAC_STRING;
            params!(Bytes, Bytes, String) => VariadicFunc::HmacBytes => Bytes, oid::FUNC_PG_HMAC_BYTES;
        },
        "initcap" => Scalar {
            params!(String) => UnaryFunc::Initcap(func::Initcap) => String, 872;
        },
        "int4range" => Scalar {
            params!(Int32, Int32) => Operation::variadic(|_ecx, mut exprs| {
                exprs.push(HirScalarExpr::literal(Datum::String("[)"), ScalarType::String));
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::Int32 },
                    exprs))
            }) => ScalarType::Range { element_type: Box::new(ScalarType::Int32)}, 3840;
            params!(Int32, Int32, String) => Operation::variadic(|_ecx, exprs| {
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::Int32 },
                    exprs))
            }) => ScalarType::Range { element_type: Box::new(ScalarType::Int32)}, 3841;
        },
        "int8range" => Scalar {
            params!(Int64, Int64) => Operation::variadic(|_ecx, mut exprs| {
                exprs.push(HirScalarExpr::literal(Datum::String("[)"), ScalarType::String));
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::Int64 },
                    exprs))
            }) => ScalarType::Range { element_type: Box::new(ScalarType::Int64)}, 3945;
            params!(Int64, Int64, String) => Operation::variadic(|_ecx, exprs| {
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::Int64 },
                    exprs))
                }) => ScalarType::Range { element_type: Box::new(ScalarType::Int64)}, 3946;
        },
        "isempty" => Scalar {
            params!(RangeAny) => UnaryFunc::RangeEmpty(func::RangeEmpty) => Bool, 3850;
        },
        "jsonb_array_length" => Scalar {
            params!(Jsonb) => UnaryFunc::JsonbArrayLength(func::JsonbArrayLength) => Int32, 3207;
        },
        "jsonb_build_array" => Scalar {
            params!() => VariadicFunc::JsonbBuildArray => Jsonb, 3272;
            params!(Any...) => Operation::variadic(|ecx, exprs| Ok(HirScalarExpr::call_variadic(VariadicFunc::JsonbBuildArray,
                exprs.into_iter().map(|e| typeconv::to_jsonb(ecx, e)).collect()))) => Jsonb, 3271;
        },
        "jsonb_build_object" => Scalar {
            params!() => VariadicFunc::JsonbBuildObject => Jsonb, 3274;
            params!(Any...) => Operation::variadic(|ecx, exprs| {
                if exprs.len() % 2 != 0 {
                    sql_bail!("argument list must have even number of elements")
                }
                Ok(HirScalarExpr::call_variadic(
                    VariadicFunc::JsonbBuildObject,
                    exprs.into_iter().tuples().map(|(key, val)| {
                        let key = typeconv::to_string(ecx, key);
                        let val = typeconv::to_jsonb(ecx, val);
                        vec![key, val]
                    }).flatten().collect()))
            }) => Jsonb, 3273;
        },
        "jsonb_pretty" => Scalar {
            params!(Jsonb) => UnaryFunc::JsonbPretty(func::JsonbPretty) => String, 3306;
        },
        "jsonb_strip_nulls" => Scalar {
            params!(Jsonb) => UnaryFunc::JsonbStripNulls(func::JsonbStripNulls) => Jsonb, 3262;
        },
        "jsonb_typeof" => Scalar {
            params!(Jsonb) => UnaryFunc::JsonbTypeof(func::JsonbTypeof) => String, 3210;
        },
        "justify_days" => Scalar {
            params!(Interval) => UnaryFunc::JustifyDays(func::JustifyDays) => Interval, 1295;
        },
        "justify_hours" => Scalar {
            params!(Interval) => UnaryFunc::JustifyHours(func::JustifyHours) => Interval, 1175;
        },
        "justify_interval" => Scalar {
            params!(Interval) => UnaryFunc::JustifyInterval(func::JustifyInterval) => Interval, 2711;
        },
        "left" => Scalar {
            params!(String, Int32) => BinaryFunc::Left => String, 3060;
        },
        "length" => Scalar {
            params!(Bytes) => UnaryFunc::ByteLengthBytes(func::ByteLengthBytes) => Int32, 2010;
            // bpcharlen is redundant with automatic coercion to string, 1318.
            params!(String) => UnaryFunc::CharLength(func::CharLength) => Int32, 1317;
            params!(Bytes, String) => BinaryFunc::EncodedBytesCharLength => Int32, 1713;
        },
        "like_escape" => Scalar {
            params!(String, String) => BinaryFunc::LikeEscape => String, 1637;
        },
        "ln" => Scalar {
            params!(Float64) => UnaryFunc::Ln(func::Ln) => Float64, 1341;
            params!(Numeric) => UnaryFunc::LnNumeric(func::LnNumeric) => Numeric, 1734;
        },
        "log10" => Scalar {
            params!(Float64) => UnaryFunc::Log10(func::Log10) => Float64, 1194;
            params!(Numeric) => UnaryFunc::Log10Numeric(func::Log10Numeric) => Numeric, 1481;
        },
        "log" => Scalar {
            params!(Float64) => UnaryFunc::Log10(func::Log10) => Float64, 1340;
            params!(Numeric) => UnaryFunc::Log10Numeric(func::Log10Numeric) => Numeric, 1741;
            params!(Numeric, Numeric) => BinaryFunc::LogNumeric => Numeric, 1736;
        },
        "lower" => Scalar {
            params!(String) => UnaryFunc::Lower(func::Lower) => String, 870;
            params!(RangeAny) => UnaryFunc::RangeLower(func::RangeLower) => AnyElement, 3848;
        },
        "lower_inc" => Scalar {
            params!(RangeAny) => UnaryFunc::RangeLowerInc(func::RangeLowerInc) => Bool, 3851;
        },
        "lower_inf" => Scalar {
            params!(RangeAny) => UnaryFunc::RangeLowerInf(func::RangeLowerInf) => Bool, 3853;
        },
        "lpad" => Scalar {
            params!(String, Int32) => VariadicFunc::PadLeading => String, 879;
            params!(String, Int32, String) => VariadicFunc::PadLeading => String, 873;
        },
        "ltrim" => Scalar {
            params!(String) => UnaryFunc::TrimLeadingWhitespace(func::TrimLeadingWhitespace) => String, 881;
            params!(String, String) => BinaryFunc::TrimLeading => String, 875;
        },
        "makeaclitem" => Scalar {
            params!(Oid, Oid, String, Bool) => VariadicFunc::MakeAclItem => AclItem, 1365;
        },
        "make_timestamp" => Scalar {
            params!(Int64, Int64, Int64, Int64, Int64, Float64) => VariadicFunc::MakeTimestamp => Timestamp, 3461;
        },
        "md5" => Scalar {
            params!(String) => Operation::unary(move |_ecx, input| {
                let algorithm = HirScalarExpr::literal(Datum::String("md5"), ScalarType::String);
                let encoding = HirScalarExpr::literal(Datum::String("hex"), ScalarType::String);
                Ok(input.call_binary(algorithm, BinaryFunc::DigestString).call_binary(encoding, BinaryFunc::Encode))
            }) => String, 2311;
            params!(Bytes) => Operation::unary(move |_ecx, input| {
                let algorithm = HirScalarExpr::literal(Datum::String("md5"), ScalarType::String);
                let encoding = HirScalarExpr::literal(Datum::String("hex"), ScalarType::String);
                Ok(input.call_binary(algorithm, BinaryFunc::DigestBytes).call_binary(encoding, BinaryFunc::Encode))
            }) => String, 2321;
        },
        "mod" => Scalar {
            params!(Numeric, Numeric) => Operation::nullary(|_ecx| catalog_name_only!("mod")) => Numeric, 1728;
            params!(Int16, Int16) => Operation::nullary(|_ecx| catalog_name_only!("mod")) => Int16, 940;
            params!(Int32, Int32) => Operation::nullary(|_ecx| catalog_name_only!("mod")) => Int32, 941;
            params!(Int64, Int64) => Operation::nullary(|_ecx| catalog_name_only!("mod")) => Int64, 947;
            params!(UInt16, UInt16) => Operation::nullary(|_ecx| catalog_name_only!("mod")) => UInt16, oid::FUNC_MOD_UINT16_OID;
            params!(UInt32, UInt32) => Operation::nullary(|_ecx| catalog_name_only!("mod")) => UInt32, oid::FUNC_MOD_UINT32_OID;
            params!(UInt64, UInt64) => Operation::nullary(|_ecx| catalog_name_only!("mod")) => UInt64, oid::FUNC_MOD_UINT64_OID;
        },
        "now" => Scalar {
            params!() => UnmaterializableFunc::CurrentTimestamp => TimestampTz, 1299;
        },
        "numrange" => Scalar {
            params!(Numeric, Numeric) => Operation::variadic(|_ecx, mut exprs| {
                exprs.push(HirScalarExpr::literal(Datum::String("[)"), ScalarType::String));
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::Numeric { max_scale: None } },
                    exprs))
            }) =>  ScalarType::Range { element_type: Box::new(ScalarType::Numeric { max_scale: None })}, 3844;
            params!(Numeric, Numeric, String) => Operation::variadic(|_ecx, exprs| {
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::Numeric { max_scale: None } },
                    exprs))
            }) => ScalarType::Range { element_type: Box::new(ScalarType::Numeric { max_scale: None })}, 3845;
        },
        "octet_length" => Scalar {
            params!(Bytes) => UnaryFunc::ByteLengthBytes(func::ByteLengthBytes) => Int32, 720;
            params!(String) => UnaryFunc::ByteLengthString(func::ByteLengthString) => Int32, 1374;
            params!(Char) => Operation::unary(|ecx, e| {
                let length = ecx.scalar_type(&e).unwrap_char_length();
                Ok(e.call_unary(UnaryFunc::PadChar(func::PadChar { length }))
                    .call_unary(UnaryFunc::ByteLengthString(func::ByteLengthString))
                )
            }) => Int32, 1375;
        },
        // SQL closely matches PostgreSQL's implementation.
        // We don't yet support casting to regnamespace, so use our constant for
        // the oid of 'pg_catalog'.
        "obj_description" => Scalar {
            params!(Oid, String) => sql_impl_func(&format!(
                "(SELECT description FROM pg_description
                  WHERE objoid = $1
                    AND classoid = (
                      SELECT oid FROM pg_class WHERE relname = $2 AND relnamespace = {})
                    AND objsubid = 0)",
                oid::SCHEMA_PG_CATALOG_OID
            )) => String, 1215;
        },
        "pg_column_size" => Scalar {
            params!(Any) => UnaryFunc::PgColumnSize(func::PgColumnSize) => Int32, 1269;
        },
        "pg_size_pretty" => Scalar {
            params!(Numeric) => UnaryFunc::PgSizePretty(func::PgSizePretty) => String, 3166;
        },
        "mz_row_size" => Scalar {
            params!(Any) => Operation::unary(|ecx, e| {
                let s = ecx.scalar_type(&e);
                if !matches!(s, ScalarType::Record{..}) {
                    sql_bail!("mz_row_size requires a record type");
                }
                Ok(e.call_unary(UnaryFunc::MzRowSize(func::MzRowSize)))
            }) => Int32, oid::FUNC_MZ_ROW_SIZE;
        },
        "parse_ident" => Scalar {
            params!(String) => Operation::unary(|_ecx, ident| {
                Ok(ident.call_binary(HirScalarExpr::literal_true(), BinaryFunc::ParseIdent))
            }) => ScalarType::Array(Box::new(ScalarType::String)),
                oid::FUNC_PARSE_IDENT_DEFAULT_STRICT;
            params!(String, Bool) => BinaryFunc::ParseIdent
                => ScalarType::Array(Box::new(ScalarType::String)), 1268;
        },
        "pg_encoding_to_char" => Scalar {
            // Materialize only supports UT8-encoded databases. Return 'UTF8' if Postgres'
            // encoding id for UTF8 (6) is provided, otherwise return 'NULL'.
            params!(Int64) => sql_impl_func("CASE WHEN $1 = 6 THEN 'UTF8' ELSE NULL END") => String, 1597;
        },
        "pg_backend_pid" => Scalar {
            params!() => UnmaterializableFunc::PgBackendPid => Int32, 2026;
        },
        // pg_get_constraintdef gives more info about a constraint within the `pg_constraint`
        // view. Certain meta commands rely on this function not throwing an error, but the
        // `pg_constraint` view is empty in materialize. Therefore we know any oid provided is
        // not a valid constraint, so we can return NULL which is what PostgreSQL does when
        // provided an invalid OID.
        "pg_get_constraintdef" => Scalar {
            params!(Oid) => Operation::unary(|_ecx, _oid|
                Ok(HirScalarExpr::literal_null(ScalarType::String))) => String, 1387;
            params!(Oid, Bool) => Operation::binary(|_ecx, _oid, _pretty|
                Ok(HirScalarExpr::literal_null(ScalarType::String))) => String, 2508;
        },
        // pg_get_indexdef reconstructs the creating command for an index. We only support
        // arrangement based indexes, so we can hardcode that in.
        // TODO(jkosh44): In order to include the index WITH options, they will need to be saved somewhere in the catalog
        "pg_get_indexdef" => Scalar {
            params!(Oid) => sql_impl_func(
                "(SELECT 'CREATE INDEX ' || i.name || ' ON ' || r.name || ' USING arrangement (' || (
                        SELECT pg_catalog.string_agg(cols.col_exp, ',' ORDER BY cols.index_position)
                        FROM (
                            SELECT c.name AS col_exp, ic.index_position
                            FROM mz_catalog.mz_index_columns AS ic
                            JOIN mz_catalog.mz_indexes AS i2 ON ic.index_id = i2.id
                            JOIN mz_catalog.mz_columns AS c ON i2.on_id = c.id AND ic.on_position = c.position
                            WHERE ic.index_id = i.id AND ic.on_expression IS NULL
                            UNION
                            SELECT ic.on_expression AS col_exp, ic.index_position
                            FROM mz_catalog.mz_index_columns AS ic
                            WHERE ic.index_id = i.id AND ic.on_expression IS NOT NULL
                        ) AS cols
                    ) || ')'
                    FROM mz_catalog.mz_indexes AS i
                    JOIN mz_catalog.mz_relations AS r ON i.on_id = r.id
                    WHERE i.oid = $1)"
            ) => String, 1643;
            // A position of 0 is treated as if no position was given.
            // Third parameter, pretty, is ignored.
            params!(Oid, Int32, Bool) => sql_impl_func(
                "(SELECT CASE WHEN $2 = 0 THEN pg_catalog.pg_get_indexdef($1) ELSE
                        (SELECT c.name
                        FROM mz_catalog.mz_indexes AS i
                        JOIN mz_catalog.mz_index_columns AS ic ON i.id = ic.index_id
                        JOIN mz_catalog.mz_columns AS c ON i.on_id = c.id AND ic.on_position = c.position
                        WHERE i.oid = $1 AND ic.on_expression IS NULL AND ic.index_position = $2
                        UNION
                        SELECT ic.on_expression
                        FROM mz_catalog.mz_indexes AS i
                        JOIN mz_catalog.mz_index_columns AS ic ON i.id = ic.index_id
                        WHERE i.oid = $1 AND ic.on_expression IS NOT NULL AND ic.index_position = $2)
                    END)"
            ) => String, 2507;
        },
        // pg_get_viewdef returns the (query part of) the given view's definition.
        // We currently don't support pretty-printing (the `Bool`/`Int32` parameters).
        "pg_get_viewdef" => Scalar {
            params!(String) => sql_impl_func(
                "(SELECT definition FROM mz_catalog.mz_views WHERE name = $1)"
            ) => String, 1640;
            params!(Oid) => sql_impl_func(
                "(SELECT definition FROM mz_catalog.mz_views WHERE oid = $1)"
            ) => String, 1641;
            params!(String, Bool) => sql_impl_func(
                "(SELECT definition FROM mz_catalog.mz_views WHERE name = $1)"
            ) => String, 2505;
            params!(Oid, Bool) => sql_impl_func(
                "(SELECT definition FROM mz_catalog.mz_views WHERE oid = $1)"
            ) => String, 2506;
            params!(Oid, Int32) => sql_impl_func(
                "(SELECT definition FROM mz_catalog.mz_views WHERE oid = $1)"
            ) => String, 3159;
        },
        // pg_get_expr is meant to convert the textual version of
        // pg_node_tree data into parseable expressions. However, we don't
        // use the pg_get_expr structure anywhere and the equivalent columns
        // in Materialize (e.g. index expressions) are already stored as
        // parseable expressions. So, we offer this function in the catalog
        // for ORM support, but make no effort to provide its semantics,
        // e.g. this also means we drop the Oid argument on the floor.
        "pg_get_expr" => Scalar {
            params!(String, Oid) => Operation::binary(|_ecx, l, _r| Ok(l)) => String, 1716;
            params!(String, Oid, Bool) => Operation::variadic(move |_ecx, mut args| Ok(args.remove(0))) => String, 2509;
        },
        "pg_get_userbyid" => Scalar {
            params!(Oid) => sql_impl_func(
                "CASE \
                   WHEN $1 IS NULL THEN NULL \
                   ELSE COALESCE(\
                     (SELECT name FROM mz_catalog.mz_roles WHERE oid = $1),\
                     'unknown (OID=' || $1 || ')'\
                   ) \
                END"
            ) => String, 1642;
        },
        // The privilege param is validated but ignored. That's because we haven't implemented
        // NOINHERIT roles, so it has no effect on the result.
        //
        // In PostgreSQL, this should always return true for superusers. In Materialize it's
        // impossible to determine if a role is a superuser since it's specific to a session. So we
        // cannot copy PostgreSQL semantics there.
        "pg_has_role" => Scalar {
            params!(String, String, String) => sql_impl_func("pg_has_role(mz_internal.mz_role_oid($1), mz_internal.mz_role_oid($2), $3)") => Bool, 2705;
            params!(String, Oid, String) => sql_impl_func("pg_has_role(mz_internal.mz_role_oid($1), $2, $3)") => Bool, 2706;
            params!(Oid, String, String) => sql_impl_func("pg_has_role($1, mz_internal.mz_role_oid($2), $3)") => Bool, 2707;
            params!(Oid, Oid, String) => sql_impl_func(
                "CASE
                -- We need to validate the privilege to return a proper error before anything
                -- else.
                WHEN NOT mz_internal.mz_validate_role_privilege($3)
                OR $1 IS NULL
                OR $2 IS NULL
                OR $3 IS NULL
                THEN NULL
                WHEN $1 NOT IN (SELECT oid FROM mz_catalog.mz_roles)
                OR $2 NOT IN (SELECT oid FROM mz_catalog.mz_roles)
                THEN false
                ELSE $2::text IN (SELECT UNNEST(mz_internal.mz_role_oid_memberships() -> $1::text))
                END",
            ) => Bool, 2708;
            params!(String, String) => sql_impl_func("pg_has_role(current_user, $1, $2)") => Bool, 2709;
            params!(Oid, String) => sql_impl_func("pg_has_role(current_user, $1, $2)") => Bool, 2710;
        },
        // pg_is_in_recovery indicates whether a recovery is still in progress. Materialize does
        // not have a concept of recovery, so we default to always returning false.
        "pg_is_in_recovery" => Scalar {
            params!() => Operation::nullary(|_ecx| {
                Ok(HirScalarExpr::literal_false())
            }) => Bool, 3810;
        },
        "pg_postmaster_start_time" => Scalar {
            params!() => UnmaterializableFunc::PgPostmasterStartTime => TimestampTz, 2560;
        },
        "pg_relation_size" => Scalar {
            params!(RegClass, String) => sql_impl_func("CASE WHEN $1 IS NULL OR $2 IS NULL THEN NULL ELSE -1::pg_catalog.int8 END") => Int64, 2332;
            params!(RegClass) => sql_impl_func("CASE WHEN $1 IS NULL THEN NULL ELSE -1::pg_catalog.int8 END") => Int64, 2325;
        },
        "pg_stat_get_numscans" => Scalar {
            params!(Oid) => sql_impl_func("CASE WHEN $1 IS NULL THEN NULL ELSE -1::pg_catalog.int8 END") => Int64, 1928;
        },
        "pg_table_is_visible" => Scalar {
            params!(Oid) => sql_impl_func(
                "(SELECT s.name = ANY(pg_catalog.current_schemas(true))
                     FROM mz_catalog.mz_objects o JOIN mz_catalog.mz_schemas s ON o.schema_id = s.id
                     WHERE o.oid = $1)"
            ) => Bool, 2079;
        },
        "pg_type_is_visible" => Scalar {
            params!(Oid) => sql_impl_func(
                "(SELECT s.name = ANY(pg_catalog.current_schemas(true))
                     FROM mz_catalog.mz_types t JOIN mz_catalog.mz_schemas s ON t.schema_id = s.id
                     WHERE t.oid = $1)"
            ) => Bool, 2080;
        },
        "pg_function_is_visible" => Scalar {
            params!(Oid) => sql_impl_func(
                "(SELECT s.name = ANY(pg_catalog.current_schemas(true))
                     FROM mz_catalog.mz_functions f JOIN mz_catalog.mz_schemas s ON f.schema_id = s.id
                     WHERE f.oid = $1)"
            ) => Bool, 2081;
        },
        // pg_tablespace_location indicates what path in the filesystem that a given tablespace is
        // located in. This concept does not make sense though in Materialize which is a cloud
        // native database, so we just return the null value.
        "pg_tablespace_location" => Scalar {
            params!(Oid) => Operation::unary(|_ecx, _e| {
                Ok(HirScalarExpr::literal_null(ScalarType::String))
            }) => String, 3778;
        },
        "pg_typeof" => Scalar {
            params!(Any) => Operation::new(|ecx, exprs, params, _order_by| {
                // pg_typeof reports the type *before* coercion.
                let name = match ecx.scalar_type(&exprs[0]) {
                    CoercibleScalarType::Uncoerced => "unknown".to_string(),
                    CoercibleScalarType::Record(_) => "record".to_string(),
                    CoercibleScalarType::Coerced(ty) => ecx.humanize_scalar_type(&ty, true),
                };

                // For consistency with other functions, verify that
                // coercion is possible, though we don't actually care about
                // the coerced results.
                coerce_args_to_types(ecx, exprs, params)?;

                // TODO(benesch): make this function have return type
                // regtype, when we support that type. Document the function
                // at that point. For now, it's useful enough to have this
                // halfway version that returns a string.
                Ok(HirScalarExpr::literal(Datum::String(&name), ScalarType::String))
            }) => String, 1619;
        },
        "position" => Scalar {
            params!(String, String) => BinaryFunc::Position => Int32, 849;
        },
        "pow" => Scalar {
            params!(Float64, Float64) => Operation::nullary(|_ecx| catalog_name_only!("pow")) => Float64, 1346;
        },
        "power" => Scalar {
            params!(Float64, Float64) => BinaryFunc::Power => Float64, 1368;
            params!(Numeric, Numeric) => BinaryFunc::PowerNumeric => Numeric, 2169;
        },
        "quote_ident" => Scalar {
            params!(String) => UnaryFunc::QuoteIdent(func::QuoteIdent) => String, 1282;
        },
        "radians" => Scalar {
            params!(Float64) => UnaryFunc::Radians(func::Radians) => Float64, 1609;
        },
        "repeat" => Scalar {
            params!(String, Int32) => BinaryFunc::RepeatString => String, 1622;
        },
        "regexp_match" => Scalar {
            params!(String, String) => VariadicFunc::RegexpMatch => ScalarType::Array(Box::new(ScalarType::String)), 3396;
            params!(String, String, String) => VariadicFunc::RegexpMatch => ScalarType::Array(Box::new(ScalarType::String)), 3397;
        },
        "replace" => Scalar {
            params!(String, String, String) => VariadicFunc::Replace => String, 2087;
        },
        "right" => Scalar {
            params!(String, Int32) => BinaryFunc::Right => String, 3061;
        },
        "round" => Scalar {
            params!(Float32) => UnaryFunc::RoundFloat32(func::RoundFloat32) => Float32, oid::FUNC_ROUND_F32_OID;
            params!(Float64) => UnaryFunc::RoundFloat64(func::RoundFloat64) => Float64, 1342;
            params!(Numeric) => UnaryFunc::RoundNumeric(func::RoundNumeric) => Numeric, 1708;
            params!(Numeric, Int32) => BinaryFunc::RoundNumeric => Numeric, 1707;
        },
        "rtrim" => Scalar {
            params!(String) => UnaryFunc::TrimTrailingWhitespace(func::TrimTrailingWhitespace) => String, 882;
            params!(String, String) => BinaryFunc::TrimTrailing => String, 876;
        },
        "sha224" => Scalar {
            params!(Bytes) => digest("sha224") => Bytes, 3419;
        },
        "sha256" => Scalar {
            params!(Bytes) => digest("sha256") => Bytes, 3420;
        },
        "sha384" => Scalar {
            params!(Bytes) => digest("sha384") => Bytes, 3421;
        },
        "sha512" => Scalar {
            params!(Bytes) => digest("sha512") => Bytes, 3422;
        },
        "sin" => Scalar {
            params!(Float64) => UnaryFunc::Sin(func::Sin) => Float64, 1604;
        },
        "asin" => Scalar {
            params!(Float64) => UnaryFunc::Asin(func::Asin) => Float64, 1600;
        },
        "sinh" => Scalar {
            params!(Float64) => UnaryFunc::Sinh(func::Sinh) => Float64, 2462;
        },
        "asinh" => Scalar {
            params!(Float64) => UnaryFunc::Asinh(func::Asinh) => Float64, 2465;
        },
        "split_part" => Scalar {
            params!(String, String, Int32) => VariadicFunc::SplitPart => String, 2088;
        },
        "stddev" => Scalar {
            params!(Float32) => Operation::nullary(|_ecx| catalog_name_only!("stddev")) => Float64, 2157;
            params!(Float64) => Operation::nullary(|_ecx| catalog_name_only!("stddev")) => Float64, 2158;
            params!(Int16) => Operation::nullary(|_ecx| catalog_name_only!("stddev")) => Numeric, 2156;
            params!(Int32) => Operation::nullary(|_ecx| catalog_name_only!("stddev")) => Numeric, 2155;
            params!(Int64) => Operation::nullary(|_ecx| catalog_name_only!("stddev")) => Numeric, 2154;
            params!(UInt16) => Operation::nullary(|_ecx| catalog_name_only!("stddev")) => Numeric, oid::FUNC_STDDEV_UINT16_OID;
            params!(UInt32) => Operation::nullary(|_ecx| catalog_name_only!("stddev")) => Numeric, oid::FUNC_STDDEV_UINT32_OID;
            params!(UInt64) => Operation::nullary(|_ecx| catalog_name_only!("stddev")) => Numeric, oid::FUNC_STDDEV_UINT64_OID;
        },
        "stddev_pop" => Scalar {
            params!(Float32) => Operation::nullary(|_ecx| catalog_name_only!("stddev_pop")) => Float64, 2727;
            params!(Float64) => Operation::nullary(|_ecx| catalog_name_only!("stddev_pop")) => Float64, 2728;
            params!(Int16) => Operation::nullary(|_ecx| catalog_name_only!("stddev_pop")) => Numeric, 2726;
            params!(Int32) => Operation::nullary(|_ecx| catalog_name_only!("stddev_pop")) => Numeric, 2725;
            params!(Int64) => Operation::nullary(|_ecx| catalog_name_only!("stddev_pop")) => Numeric, 2724;
            params!(UInt16) => Operation::nullary(|_ecx| catalog_name_only!("stddev_pop")) => Numeric, oid::FUNC_STDDEV_POP_UINT16_OID;
            params!(UInt32) => Operation::nullary(|_ecx| catalog_name_only!("stddev_pop")) => Numeric, oid::FUNC_STDDEV_POP_UINT32_OID;
            params!(UInt64) => Operation::nullary(|_ecx| catalog_name_only!("stddev_pop")) => Numeric, oid::FUNC_STDDEV_POP_UINT64_OID;
        },
        "stddev_samp" => Scalar {
            params!(Float32) => Operation::nullary(|_ecx| catalog_name_only!("stddev_samp")) => Float64, 2715;
            params!(Float64) => Operation::nullary(|_ecx| catalog_name_only!("stddev_samp")) => Float64, 2716;
            params!(Int16) => Operation::nullary(|_ecx| catalog_name_only!("stddev_samp")) => Numeric, 2714;
            params!(Int32) => Operation::nullary(|_ecx| catalog_name_only!("stddev_samp")) => Numeric, 2713;
            params!(Int64) => Operation::nullary(|_ecx| catalog_name_only!("stddev_samp")) => Numeric, 2712;
            params!(UInt16) => Operation::nullary(|_ecx| catalog_name_only!("stddev_samp")) => Numeric, oid::FUNC_STDDEV_SAMP_UINT16_OID;
            params!(UInt32) => Operation::nullary(|_ecx| catalog_name_only!("stddev_samp")) => Numeric, oid::FUNC_STDDEV_SAMP_UINT32_OID;
            params!(UInt64) => Operation::nullary(|_ecx| catalog_name_only!("stddev_samp")) => Numeric, oid::FUNC_STDDEV_SAMP_UINT64_OID;
        },
        "substr" => Scalar {
            params!(String, Int32) => VariadicFunc::Substr => String, 883;
            params!(String, Int32, Int32) => VariadicFunc::Substr => String, 877;
        },
        "substring" => Scalar {
            params!(String, Int32) => VariadicFunc::Substr => String, 937;
            params!(String, Int32, Int32) => VariadicFunc::Substr => String, 936;
        },
        "sqrt" => Scalar {
            params!(Float64) => UnaryFunc::SqrtFloat64(func::SqrtFloat64) => Float64, 1344;
            params!(Numeric) => UnaryFunc::SqrtNumeric(func::SqrtNumeric) => Numeric, 1730;
        },
        "tan" => Scalar {
            params!(Float64) => UnaryFunc::Tan(func::Tan) => Float64, 1606;
        },
        "atan" => Scalar {
            params!(Float64) => UnaryFunc::Atan(func::Atan) => Float64, 1602;
        },
        "tanh" => Scalar {
            params!(Float64) => UnaryFunc::Tanh(func::Tanh) => Float64, 2464;
        },
        "atanh" => Scalar {
            params!(Float64) => UnaryFunc::Atanh(func::Atanh) => Float64, 2467;
        },
        "age" => Scalar {
            params!(Timestamp, Timestamp) => BinaryFunc::AgeTimestamp => Interval, 2058;
            params!(TimestampTz, TimestampTz) => BinaryFunc::AgeTimestampTz => Interval, 1199;
        },
        "timezone" => Scalar {
            params!(String, Timestamp) => BinaryFunc::TimezoneTimestamp => TimestampTz, 2069;
            params!(String, TimestampTz) => BinaryFunc::TimezoneTimestampTz => Timestamp, 1159;
            // PG defines this as `text timetz`
            params!(String, Time) => Operation::binary(|ecx, lhs, rhs| {
                // NOTE: this overload is wrong. It should take and return a
                // `timetz`, which is a type we don't support because it has
                // inscrutable semantics (timezones are meaningless without a
                // date). This implementation attempted to extend those already
                // inscrutable semantics to the `time` type, which makes matters
                // even worse.
                //
                // This feature flag ensures we don't get *new* uses of this
                // function. At some point in the future, we should either
                // remove this overload entirely, after validating there are no
                // catalogs in production that rely on this overload, or we
                // should properly support the `timetz` type and adjust this
                // overload accordingly.
                ecx.require_feature_flag(&ENABLE_TIME_AT_TIME_ZONE)?;
                Ok(HirScalarExpr::call_variadic(
                    VariadicFunc::TimezoneTime,
                    vec![
                        lhs,
                        rhs,
                        HirScalarExpr::call_unmaterializable(UnmaterializableFunc::CurrentTimestamp),
                    ],
                ))
            }) => Time, 2037;
            params!(Interval, Timestamp) => BinaryFunc::TimezoneIntervalTimestamp => TimestampTz, 2070;
            params!(Interval, TimestampTz) => BinaryFunc::TimezoneIntervalTimestampTz => Timestamp, 1026;
            // PG defines this as `interval timetz`
            params!(Interval, Time) => BinaryFunc::TimezoneIntervalTime => Time, 2038;
        },
        "to_char" => Scalar {
            params!(Timestamp, String) => BinaryFunc::ToCharTimestamp => String, 2049;
            params!(TimestampTz, String) => BinaryFunc::ToCharTimestampTz => String, 1770;
        },
        // > Returns the value as json or jsonb. Arrays and composites
        // > are converted (recursively) to arrays and objects;
        // > otherwise, if there is a cast from the type to json, the
        // > cast function will be used to perform the conversion;
        // > otherwise, a scalar value is produced. For any scalar type
        // > other than a number, a Boolean, or a null value, the text
        // > representation will be used, in such a fashion that it is a
        // > valid json or jsonb value.
        //
        // https://www.postgresql.org/docs/current/functions-json.html
        "to_jsonb" => Scalar {
            params!(Any) => Operation::unary(|ecx, e| {
                // TODO(see <materialize#7572>): remove this
                let e = match ecx.scalar_type(&e) {
                    ScalarType::Char { length } => e.call_unary(UnaryFunc::PadChar(func::PadChar { length })),
                    _ => e,
                };
                Ok(typeconv::to_jsonb(ecx, e))
            }) => Jsonb, 3787;
        },
        "to_timestamp" => Scalar {
            params!(Float64) => UnaryFunc::ToTimestamp(func::ToTimestamp) => TimestampTz, 1158;
        },
        "translate" => Scalar {
            params!(String, String, String) => VariadicFunc::Translate => String, 878;
        },
        "trunc" => Scalar {
            params!(Float32) => UnaryFunc::TruncFloat32(func::TruncFloat32) => Float32, oid::FUNC_TRUNC_F32_OID;
            params!(Float64) => UnaryFunc::TruncFloat64(func::TruncFloat64) => Float64, 1343;
            params!(Numeric) => UnaryFunc::TruncNumeric(func::TruncNumeric) => Numeric, 1710;
        },
        "tsrange" => Scalar {
            params!(Timestamp, Timestamp) => Operation::variadic(|_ecx, mut exprs| {
                exprs.push(HirScalarExpr::literal(Datum::String("[)"), ScalarType::String));
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::Timestamp {precision: None}},
                    exprs))
            }) =>  ScalarType::Range { element_type: Box::new(ScalarType::Timestamp { precision: None})}, 3933;
            params!(Timestamp, Timestamp, String) => Operation::variadic(|_ecx, exprs| {
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::Timestamp {precision: None}},
                    exprs))
            }) => ScalarType::Range { element_type: Box::new(ScalarType::Timestamp { precision: None})}, 3934;
        },
        "tstzrange" => Scalar {
            params!(TimestampTz, TimestampTz) => Operation::variadic(|_ecx, mut exprs| {
                exprs.push(HirScalarExpr::literal(Datum::String("[)"), ScalarType::String));
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::TimestampTz {precision: None}},
                    exprs,))
            }) =>  ScalarType::Range { element_type: Box::new(ScalarType::TimestampTz { precision: None})}, 3937;
            params!(TimestampTz, TimestampTz, String) => Operation::variadic(|_ecx, exprs| {
                Ok(HirScalarExpr::call_variadic(VariadicFunc::RangeCreate { elem_type: ScalarType::TimestampTz {precision: None}},
                    exprs))
            }) => ScalarType::Range { element_type: Box::new(ScalarType::TimestampTz { precision: None})}, 3938;
        },
        "upper" => Scalar {
            params!(String) => UnaryFunc::Upper(func::Upper) => String, 871;
            params!(RangeAny) => UnaryFunc::RangeUpper(func::RangeUpper) => AnyElement, 3849;
        },
        "upper_inc" => Scalar {
            params!(RangeAny) => UnaryFunc::RangeUpperInc(func::RangeUpperInc) => Bool, 3852;
        },
        "upper_inf" => Scalar {
            params!(RangeAny) => UnaryFunc::RangeUpperInf(func::RangeUpperInf) => Bool, 3854;
        },
        "uuid_generate_v5" => Scalar {
            params!(Uuid, String) => BinaryFunc::UuidGenerateV5 => Uuid, oid::FUNC_PG_UUID_GENERATE_V5;
        },
        "variance" => Scalar {
            params!(Float32) => Operation::nullary(|_ecx| catalog_name_only!("variance")) => Float64, 2151;
            params!(Float64) => Operation::nullary(|_ecx| catalog_name_only!("variance")) => Float64, 2152;
            params!(Int16) => Operation::nullary(|_ecx| catalog_name_only!("variance")) => Numeric, 2150;
            params!(Int32) => Operation::nullary(|_ecx| catalog_name_only!("variance")) => Numeric, 2149;
            params!(Int64) => Operation::nullary(|_ecx| catalog_name_only!("variance")) => Numeric, 2148;
            params!(UInt16) => Operation::nullary(|_ecx| catalog_name_only!("variance")) => Numeric, oid::FUNC_VARIANCE_UINT16_OID;
            params!(UInt32) => Operation::nullary(|_ecx| catalog_name_only!("variance")) => Numeric, oid::FUNC_VARIANCE_UINT32_OID;
            params!(UInt64) => Operation::nullary(|_ecx| catalog_name_only!("variance")) => Numeric, oid::FUNC_VARIANCE_UINT64_OID;
        },
        "var_pop" => Scalar {
            params!(Float32) => Operation::nullary(|_ecx| catalog_name_only!("var_pop")) => Float64, 2721;
            params!(Float64) => Operation::nullary(|_ecx| catalog_name_only!("var_pop")) => Float64, 2722;
            params!(Int16) => Operation::nullary(|_ecx| catalog_name_only!("var_pop")) => Numeric, 2720;
            params!(Int32) => Operation::nullary(|_ecx| catalog_name_only!("var_pop")) => Numeric, 2719;
            params!(Int64) => Operation::nullary(|_ecx| catalog_name_only!("var_pop")) => Numeric, 2718;
            params!(UInt16) => Operation::nullary(|_ecx| catalog_name_only!("var_pop")) => Numeric, oid::FUNC_VAR_POP_UINT16_OID;
            params!(UInt32) => Operation::nullary(|_ecx| catalog_name_only!("var_pop")) => Numeric, oid::FUNC_VAR_POP_UINT32_OID;
            params!(UInt64) => Operation::nullary(|_ecx| catalog_name_only!("var_pop")) => Numeric, oid::FUNC_VAR_POP_UINT64_OID;
        },
        "var_samp" => Scalar {
            params!(Float32) => Operation::nullary(|_ecx| catalog_name_only!("var_samp")) => Float64, 2644;
            params!(Float64) => Operation::nullary(|_ecx| catalog_name_only!("var_samp")) => Float64, 2645;
            params!(Int16) => Operation::nullary(|_ecx| catalog_name_only!("var_samp")) => Numeric, 2643;
            params!(Int32) => Operation::nullary(|_ecx| catalog_name_only!("var_samp")) => Numeric, 2642;
            params!(Int64) => Operation::nullary(|_ecx| catalog_name_only!("var_samp")) => Numeric, 2641;
            params!(UInt16) => Operation::nullary(|_ecx| catalog_name_only!("var_samp")) => Numeric, oid::FUNC_VAR_SAMP_UINT16_OID;
            params!(UInt32) => Operation::nullary(|_ecx| catalog_name_only!("var_samp")) => Numeric, oid::FUNC_VAR_SAMP_UINT32_OID;
            params!(UInt64) => Operation::nullary(|_ecx| catalog_name_only!("var_samp")) => Numeric, oid::FUNC_VAR_SAMP_UINT64_OID;
        },
        "version" => Scalar {
            params!() => UnmaterializableFunc::Version => String, 89;
        },

        // Internal conversion stubs.
        "aclitemin" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("aclitemin")) => AclItem, 1031;
        },
        "any_in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("any_in")) => Any, 2294;
        },
        "anyarray_in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("anyarray_in")) => ArrayAny, 2296;
        },
        "anycompatible_in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("anycompatible_in")) => AnyCompatible, 5086;
        },
        "anycompatiblearray_in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("anycompatiblearray_in")) => ArrayAnyCompatible, 5088;
        },
        "anycompatiblenonarray_in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("anycompatiblenonarray_in")) => NonVecAnyCompatible, 5092;
        },
        "anycompatiblerange_in" => Scalar {
            params!(String, Oid, Int32) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("anycompatiblerange_in")) => RangeAnyCompatible, 5094;
        },
        "anyelement_in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("anyelement_in")) => AnyElement, 2312;
        },
        "anynonarray_in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("anynonarray_in")) => NonVecAny, 2777;
        },
        "anyrange_in" => Scalar {
            params!(String, Oid, Int32) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("anyrange_in")) => RangeAny, 3832;
        },
        "array_in" => Scalar {
            params!(String, Oid, Int32) =>
                Operation::variadic(|_ecx, _exprs| bail_unsupported!("array_in")) => ArrayAny, 750;
        },
        "boolin" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("boolin")) => Bool, 1242;
        },
        "bpcharin" => Scalar {
            params!(String, Oid, Int32) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("bpcharin")) => Char, 1044;
        },
        "byteain" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("byteain")) => Bytes, 1244;
        },
        "charin" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("charin")) => PgLegacyChar, 1245;
        },
        "date_in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("date_in")) => Date, 1084;
        },
        "float4in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("float4in")) => Float32, 200;
        },
        "float8in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("float8in")) => Float64, 214;
        },
        "int2in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("int2in")) => Int16, 38;
        },
        "int2vectorin" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("int2vectorin")) => Int2Vector, 40;
        },
        "int4in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("int4in")) => Int32, 42;
        },
        "int8in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("int8in")) => Int64, 460;
        },
        "internal_in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("internal_in")) => Internal, 2304;
        },
        "interval_in" => Scalar {
            params!(String, Oid, Int32) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("interval_in")) => Interval, 1160;
        },
        "jsonb_in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("jsonb_in")) => Jsonb, 3806;
        },
        "namein" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("namein")) => PgLegacyName, 34;
        },
        "numeric_in" => Scalar {
            params!(String, Oid, Int32) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("numeric_in")) => Numeric, 1701;
        },
        "oidin" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("oidin")) => Oid, 1798;
        },
        "range_in" => Scalar {
            params!(String, Oid, Int32) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("range_in")) => RangeAny, 3834;
        },
        "record_in" => Scalar {
            params!(String, Oid, Int32) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("record_in")) => RecordAny, 2290;
        },
        "regclassin" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("regclassin")) => RegClass, 2218;
        },
        "regprocin" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("regprocin")) => RegProc, 44;
        },
        "regtypein" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("regtypein")) => RegType, 2220;
        },
        "textin" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("textin")) => String, 46;
        },
        "time_in" => Scalar {
            params!(String, Oid, Int32) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("time_in")) => Time, 1143;
        },
        "timestamp_in" => Scalar {
            params!(String, Oid, Int32) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("timestamp_in")) => Timestamp, 1312;
        },
        "timestamptz_in" => Scalar {
            params!(String, Oid, Int32) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("timestamptz_in")) => TimestampTz, 1150;
        },
        "varcharin" => Scalar {
            params!(String, Oid, Int32) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("varcharin")) => VarChar, 1046;
        },
        "uuid_in" => Scalar {
            params!(String) => Operation::variadic(|_ecx, _exprs| bail_unsupported!("uuid_in")) => Uuid, 2952;
        },
        "boolrecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("boolrecv")) => Bool, 2436;
        },
        "textrecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("textrecv")) => String, 2414;
        },
        "anyarray_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("anyarray_recv")) => ArrayAny, 2502;
        },
        "bytearecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("bytearecv")) => Bytes, 2412;
        },
        "bpcharrecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("bpcharrecv")) => Char, 2430;
        },
        "charrecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("charrecv")) => PgLegacyChar, 2434;
        },
        "date_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("date_recv")) => Date, 2468;
        },
        "float4recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("float4recv")) => Float32, 2424;
        },
        "float8recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("float8recv")) => Float64, 2426;
        },
        "int4recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("int4recv")) => Int32, 2406;
        },
        "int8recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("int8recv")) => Int64, 2408;
        },
        "interval_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("interval_recv")) => Interval, 2478;
        },
        "jsonb_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("jsonb_recv")) => Jsonb, 3805;
        },
        "namerecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("namerecv")) => PgLegacyName, 2422;
        },
        "numeric_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("numeric_recv")) => Numeric, 2460;
        },
        "oidrecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("oidrecv")) => Oid, 2418;
        },
        "record_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("recordrerecord_recvcv")) => RecordAny, 2402;
        },
        "regclassrecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("regclassrecv")) => RegClass, 2452;
        },
        "regprocrecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("regprocrecv")) => RegProc, 2444;
        },
        "regtyperecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("regtyperecv")) => RegType, 2454;
        },
        "int2recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("int2recv")) => Int16, 2404;
        },
        "time_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("time_recv")) => Time, 2470;
        },
        "timestamp_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("timestamp_recv")) => Timestamp, 2474;
        },
        "timestamptz_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("timestamptz_recv")) => TimestampTz, 2476;
        },
        "uuid_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("uuid_recv")) => Uuid, 2961;
        },
        "varcharrecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("varcharrecv")) => VarChar, 2432;
        },
        "int2vectorrecv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("int2vectorrecv")) => Int2Vector, 2410;
        },
        "anycompatiblearray_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("anycompatiblearray_recv")) => ArrayAnyCompatible, 5090;
        },
        "array_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("array_recv")) => ArrayAny, 2400;
        },
        "range_recv" => Scalar {
            params!(Internal) => Operation::nullary(|_ecx| catalog_name_only!("range_recv")) => RangeAny, 3836;
        },


        // Aggregates.
        "array_agg" => Aggregate {
            params!(NonVecAny) => Operation::unary_ordered(|ecx, e, order_by| {
                let elem_type = ecx.scalar_type(&e);

                let elem_type = match elem_type.array_of_self_elem_type() {
                    Ok(elem_type) => elem_type,
                    Err(elem_type) => bail_unsupported!(
                        format!("array_agg on {}", ecx.humanize_scalar_type(&elem_type, false))
                    ),
                };

                // ArrayConcat excepts all inputs to be arrays, so wrap all input datums into
                // arrays.
                let e_arr = HirScalarExpr::call_variadic(
                    VariadicFunc::ArrayCreate { elem_type },
                    vec![e],
                );
                Ok((e_arr, AggregateFunc::ArrayConcat { order_by }))
            }) => ArrayAny, 2335;
            params!(ArrayAny) => Operation::unary(|_ecx, _e| bail_unsupported!("array_agg on arrays")) => ArrayAny, 4053;
        },
        "bool_and" => Aggregate {
            params!(Bool) => Operation::nullary(|_ecx| catalog_name_only!("bool_and")) => Bool, 2517;
        },
        "bool_or" => Aggregate {
            params!(Bool) => Operation::nullary(|_ecx| catalog_name_only!("bool_or")) => Bool, 2518;
        },
        "count" => Aggregate {
            params!() => Operation::nullary(|_ecx| {
                // COUNT(*) is equivalent to COUNT(true).
                // This is mirrored in `AggregateExpr::is_count_asterisk`, so if you modify this,
                // then attend to that code also (in both HIR and MIR).
                Ok((HirScalarExpr::literal_true(), AggregateFunc::Count))
            }) => Int64, 2803;
            params!(Any) => AggregateFunc::Count => Int64, 2147;
        },
        "max" => Aggregate {
            params!(Bool) => AggregateFunc::MaxBool => Bool, oid::FUNC_MAX_BOOL_OID;
            params!(Int16) => AggregateFunc::MaxInt16 => Int16, 2117;
            params!(Int32) => AggregateFunc::MaxInt32 => Int32, 2116;
            params!(Int64) => AggregateFunc::MaxInt64 => Int64, 2115;
            params!(UInt16) => AggregateFunc::MaxUInt16 => UInt16, oid::FUNC_MAX_UINT16_OID;
            params!(UInt32) => AggregateFunc::MaxUInt32 => UInt32, oid::FUNC_MAX_UINT32_OID;
            params!(UInt64) => AggregateFunc::MaxUInt64 => UInt64, oid::FUNC_MAX_UINT64_OID;
            params!(MzTimestamp) => AggregateFunc::MaxMzTimestamp => MzTimestamp, oid::FUNC_MAX_MZ_TIMESTAMP_OID;
            params!(Float32) => AggregateFunc::MaxFloat32 => Float32, 2119;
            params!(Float64) => AggregateFunc::MaxFloat64 => Float64, 2120;
            params!(String) => AggregateFunc::MaxString => String, 2129;
            // TODO(see <materialize#7572>): make this its own function
            params!(Char) => AggregateFunc::MaxString => Char, 2244;
            params!(Date) => AggregateFunc::MaxDate => Date, 2122;
            params!(Timestamp) => AggregateFunc::MaxTimestamp => Timestamp, 2126;
            params!(TimestampTz) => AggregateFunc::MaxTimestampTz => TimestampTz, 2127;
            params!(Numeric) => AggregateFunc::MaxNumeric => Numeric, oid::FUNC_MAX_NUMERIC_OID;
            params!(Interval) => AggregateFunc::MaxInterval => Interval, 2128;
            params!(Time) => AggregateFunc::MaxTime => Time, 2123;
        },
        "min" => Aggregate {
            params!(Bool) => AggregateFunc::MinBool => Bool, oid::FUNC_MIN_BOOL_OID;
            params!(Int16) => AggregateFunc::MinInt16 => Int16, 2133;
            params!(Int32) => AggregateFunc::MinInt32 => Int32, 2132;
            params!(Int64) => AggregateFunc::MinInt64 => Int64, 2131;
            params!(UInt16) => AggregateFunc::MinUInt16 => UInt16, oid::FUNC_MIN_UINT16_OID;
            params!(UInt32) => AggregateFunc::MinUInt32 => UInt32, oid::FUNC_MIN_UINT32_OID;
            params!(UInt64) => AggregateFunc::MinUInt64 => UInt64, oid::FUNC_MIN_UINT64_OID;
            params!(MzTimestamp) => AggregateFunc::MinMzTimestamp => MzTimestamp, oid::FUNC_MIN_MZ_TIMESTAMP_OID;
            params!(Float32) => AggregateFunc::MinFloat32 => Float32, 2135;
            params!(Float64) => AggregateFunc::MinFloat64 => Float64, 2136;
            params!(String) => AggregateFunc::MinString => String, 2145;
            // TODO(see <materialize#7572>): make this its own function
            params!(Char) => AggregateFunc::MinString => Char, 2245;
            params!(Date) => AggregateFunc::MinDate => Date, 2138;
            params!(Timestamp) => AggregateFunc::MinTimestamp => Timestamp, 2142;
            params!(TimestampTz) => AggregateFunc::MinTimestampTz => TimestampTz, 2143;
            params!(Numeric) => AggregateFunc::MinNumeric => Numeric, oid::FUNC_MIN_NUMERIC_OID;
            params!(Interval) => AggregateFunc::MinInterval => Interval, 2144;
            params!(Time) => AggregateFunc::MinTime => Time, 2139;
        },
        "jsonb_agg" => Aggregate {
            params!(Any) => Operation::unary_ordered(|ecx, e, order_by| {
                // TODO(see <materialize#7572>): remove this
                let e = match ecx.scalar_type(&e) {
                    ScalarType::Char { length } => e.call_unary(UnaryFunc::PadChar(func::PadChar { length })),
                    _ => e,
                };
                // `AggregateFunc::JsonbAgg` filters out `Datum::Null` (it
                // needs to have *some* identity input), but the semantics
                // of the SQL function require that `Datum::Null` is treated
                // as `Datum::JsonbNull`. This call to `coalesce` converts
                // between the two semantics.
                let json_null = HirScalarExpr::literal(Datum::JsonNull, ScalarType::Jsonb);
                let e = HirScalarExpr::call_variadic(
                    VariadicFunc::Coalesce,
                    vec![typeconv::to_jsonb(ecx, e), json_null],
                );
                Ok((e, AggregateFunc::JsonbAgg { order_by }))
            }) => Jsonb, 3267;
        },
        "jsonb_object_agg" => Aggregate {
            params!(Any, Any) => Operation::binary_ordered(|ecx, key, val, order_by| {
                // TODO(see <materialize#7572>): remove this
                let key = match ecx.scalar_type(&key) {
                    ScalarType::Char { length } => key.call_unary(UnaryFunc::PadChar(func::PadChar { length })),
                    _ => key,
                };
                let val = match ecx.scalar_type(&val) {
                    ScalarType::Char { length } => val.call_unary(UnaryFunc::PadChar(func::PadChar { length })),
                    _ => val,
                };

                let json_null = HirScalarExpr::literal(Datum::JsonNull, ScalarType::Jsonb);
                let key = typeconv::to_string(ecx, key);
                // `AggregateFunc::JsonbObjectAgg` uses the same underlying
                // implementation as `AggregateFunc::MapAgg`, so it's our
                // responsibility to apply the JSON-specific behavior of casting
                // SQL nulls to JSON nulls; otherwise the produced `Datum::Map`
                // can contain `Datum::Null` values that are not valid for the
                // `ScalarType::Jsonb` type.
                let val = HirScalarExpr::call_variadic(
                    VariadicFunc::Coalesce,
                    vec![typeconv::to_jsonb(ecx, val), json_null],
                );
                let e = HirScalarExpr::call_variadic(
                    VariadicFunc::RecordCreate {
                        field_names: vec![ColumnName::from("key"), ColumnName::from("val")],
                    },
                    vec![key, val],
                );
                Ok((e, AggregateFunc::JsonbObjectAgg { order_by }))
            }) => Jsonb, 3270;
        },
        "string_agg" => Aggregate {
            params!(String, String) => Operation::binary_ordered(|_ecx, value, sep, order_by| {
                let e = HirScalarExpr::call_variadic(
                    VariadicFunc::RecordCreate {
                        field_names: vec![ColumnName::from("value"), ColumnName::from("sep")],
                    },
                    vec![value, sep],
                );
                Ok((e, AggregateFunc::StringAgg { order_by }))
            }) => String, 3538;
            params!(Bytes, Bytes) => Operation::binary(|_ecx, _l, _r| bail_unsupported!("string_agg on BYTEA")) => Bytes, 3545;
        },
        "string_to_array" => Scalar {
            params!(String, String) => VariadicFunc::StringToArray => ScalarType::Array(Box::new(ScalarType::String)), 376;
            params!(String, String, String) => VariadicFunc::StringToArray => ScalarType::Array(Box::new(ScalarType::String)), 394;
        },
        "sum" => Aggregate {
            params!(Int16) => AggregateFunc::SumInt16 => Int64, 2109;
            params!(Int32) => AggregateFunc::SumInt32 => Int64, 2108;
            params!(Int64) => AggregateFunc::SumInt64 => Numeric, 2107;
            params!(UInt16) => AggregateFunc::SumUInt16 => UInt64, oid::FUNC_SUM_UINT16_OID;
            params!(UInt32) => AggregateFunc::SumUInt32 => UInt64, oid::FUNC_SUM_UINT32_OID;
            params!(UInt64) => AggregateFunc::SumUInt64 => Numeric, oid::FUNC_SUM_UINT64_OID;
            params!(Float32) => AggregateFunc::SumFloat32 => Float32, 2110;
            params!(Float64) => AggregateFunc::SumFloat64 => Float64, 2111;
            params!(Numeric) => AggregateFunc::SumNumeric => Numeric, 2114;
            params!(Interval) => Operation::unary(|_ecx, _e| {
                // Explicitly providing this unsupported overload
                // prevents `sum(NULL)` from choosing the `Float64`
                // implementation, so that we match PostgreSQL's behavior.
                // Plus we will one day want to support this overload.
                bail_unsupported!("sum(interval)");
            }) => Interval, 2113;
        },

        // Scalar window functions.
        "row_number" => ScalarWindow {
            params!() => ScalarWindowFunc::RowNumber => Int64, 3100;
        },
        "rank" => ScalarWindow {
            params!() => ScalarWindowFunc::Rank => Int64, 3101;
        },
        "dense_rank" => ScalarWindow {
            params!() => ScalarWindowFunc::DenseRank => Int64, 3102;
        },
        "lag" => ValueWindow {
            // All args are encoded into a single record to be handled later
            params!(AnyElement) => Operation::unary(|ecx, e| {
                let typ = ecx.scalar_type(&e);
                let e = HirScalarExpr::call_variadic(
                    VariadicFunc::RecordCreate {
                        field_names: vec![ColumnName::from("expr"), ColumnName::from("offset"), ColumnName::from("default")],
                    },
                    vec![e, HirScalarExpr::literal(Datum::Int32(1), ScalarType::Int32), HirScalarExpr::literal_null(typ)],
                );
                Ok((e, ValueWindowFunc::Lag))
            }) => AnyElement, 3106;
            params!(AnyElement, Int32) => Operation::binary(|ecx, e, offset| {
                let typ = ecx.scalar_type(&e);
                let e = HirScalarExpr::call_variadic(
                    VariadicFunc::RecordCreate {
                        field_names: vec![ColumnName::from("expr"), ColumnName::from("offset"), ColumnName::from("default")],
                    },
                    vec![e, offset, HirScalarExpr::literal_null(typ)],
                );
                Ok((e, ValueWindowFunc::Lag))
            }) => AnyElement, 3107;
            params!(AnyCompatible, Int32, AnyCompatible) => Operation::variadic(|_ecx, exprs| {
                let e = HirScalarExpr::call_variadic(
                    VariadicFunc::RecordCreate {
                        field_names: vec![ColumnName::from("expr"), ColumnName::from("offset"), ColumnName::from("default")],
                    },
                    exprs,
                );
                Ok((e, ValueWindowFunc::Lag))
            }) => AnyCompatible, 3108;
        },
        "lead" => ValueWindow {
            // All args are encoded into a single record to be handled later
            params!(AnyElement) => Operation::unary(|ecx, e| {
                let typ = ecx.scalar_type(&e);
                let e = HirScalarExpr::call_variadic(
                    VariadicFunc::RecordCreate {
                        field_names: vec![ColumnName::from("expr"), ColumnName::from("offset"), ColumnName::from("default")],
                    },
                    vec![e, HirScalarExpr::literal(Datum::Int32(1), ScalarType::Int32), HirScalarExpr::literal_null(typ)],
                );
                Ok((e, ValueWindowFunc::Lead))
            }) => AnyElement, 3109;
            params!(AnyElement, Int32) => Operation::binary(|ecx, e, offset| {
                let typ = ecx.scalar_type(&e);
                let e = HirScalarExpr::call_variadic(
                    VariadicFunc::RecordCreate {
                        field_names: vec![ColumnName::from("expr"), ColumnName::from("offset"), ColumnName::from("default")],
                    },
                    vec![e, offset, HirScalarExpr::literal_null(typ)],
                );
                Ok((e, ValueWindowFunc::Lead))
            }) => AnyElement, 3110;
            params!(AnyCompatible, Int32, AnyCompatible) => Operation::variadic(|_ecx, exprs| {
                let e = HirScalarExpr::call_variadic(
                    VariadicFunc::RecordCreate {
                        field_names: vec![ColumnName::from("expr"), ColumnName::from("offset"), ColumnName::from("default")],
                    },
                    exprs,
                );
                Ok((e, ValueWindowFunc::Lead))
            }) => AnyCompatible, 3111;
        },
        "first_value" => ValueWindow {
            params!(AnyElement) => ValueWindowFunc::FirstValue => AnyElement, 3112;
        },
        "last_value" => ValueWindow {
            params!(AnyElement) => ValueWindowFunc::LastValue => AnyElement, 3113;
        },

        // Table functions.
        "generate_series" => Table {
            params!(Int32, Int32, Int32) => Operation::variadic(move |_ecx, exprs| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::GenerateSeriesInt32,
                        exprs,
                    },
                    column_names: vec!["generate_series".into()],
                })
            }) => ReturnType::set_of(Int32.into()), 1066;
            params!(Int32, Int32) => Operation::binary(move |_ecx, start, stop| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::GenerateSeriesInt32,
                        exprs: vec![start, stop, HirScalarExpr::literal(Datum::Int32(1), ScalarType::Int32)],
                    },
                    column_names: vec!["generate_series".into()],
                })
            }) => ReturnType::set_of(Int32.into()), 1067;
            params!(Int64, Int64, Int64) => Operation::variadic(move |_ecx, exprs| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::GenerateSeriesInt64,
                        exprs,
                    },
                    column_names: vec!["generate_series".into()],
                })
            }) => ReturnType::set_of(Int64.into()), 1068;
            params!(Int64, Int64) => Operation::binary(move |_ecx, start, stop| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::GenerateSeriesInt64,
                        exprs: vec![start, stop, HirScalarExpr::literal(Datum::Int64(1), ScalarType::Int64)],
                    },
                    column_names: vec!["generate_series".into()],
                })
            }) => ReturnType::set_of(Int64.into()), 1069;
            params!(Timestamp, Timestamp, Interval) => Operation::variadic(move |_ecx, exprs| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::GenerateSeriesTimestamp,
                        exprs,
                    },
                    column_names: vec!["generate_series".into()],
                })
            }) => ReturnType::set_of(Timestamp.into()), 938;
            params!(TimestampTz, TimestampTz, Interval) => Operation::variadic(move |_ecx, exprs| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::GenerateSeriesTimestampTz,
                        exprs,
                    },
                    column_names: vec!["generate_series".into()],
                })
            }) => ReturnType::set_of(TimestampTz.into()), 939;
        },

        "generate_subscripts" => Table {
            params!(ArrayAny, Int32) => Operation::variadic(move |_ecx, exprs| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::GenerateSubscriptsArray,
                        exprs,
                    },
                    column_names: vec!["generate_subscripts".into()],
                })
            }) => ReturnType::set_of(Int32.into()), 1192;
        },

        "jsonb_array_elements" => Table {
            params!(Jsonb) => Operation::unary(move |_ecx, jsonb| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::JsonbArrayElements { stringify: false },
                        exprs: vec![jsonb],
                    },
                    column_names: vec!["value".into()],
                })
            }) => ReturnType::set_of(Jsonb.into()), 3219;
        },
        "jsonb_array_elements_text" => Table {
            params!(Jsonb) => Operation::unary(move |_ecx, jsonb| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::JsonbArrayElements { stringify: true },
                        exprs: vec![jsonb],
                    },
                    column_names: vec!["value".into()],
                })
            }) => ReturnType::set_of(String.into()), 3465;
        },
        "jsonb_each" => Table {
            params!(Jsonb) => Operation::unary(move |_ecx, jsonb| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::JsonbEach { stringify: false },
                        exprs: vec![jsonb],
                    },
                    column_names: vec!["key".into(), "value".into()],
                })
            }) => ReturnType::set_of(RecordAny), 3208;
        },
        "jsonb_each_text" => Table {
            params!(Jsonb) => Operation::unary(move |_ecx, jsonb| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::JsonbEach { stringify: true },
                        exprs: vec![jsonb],
                    },
                    column_names: vec!["key".into(), "value".into()],
                })
            }) => ReturnType::set_of(RecordAny), 3932;
        },
        "jsonb_object_keys" => Table {
            params!(Jsonb) => Operation::unary(move |_ecx, jsonb| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::JsonbObjectKeys,
                        exprs: vec![jsonb],
                    },
                    column_names: vec!["jsonb_object_keys".into()],
                })
            }) => ReturnType::set_of(String.into()), 3931;
        },
        // Note that these implementations' input to `generate_series` is
        // contrived to match Flink's expected values. There are other,
        // equally valid windows we could generate.
        "date_bin_hopping" => Table {
            // (hop, width, timestamp)
            params!(Interval, Interval, Timestamp) => experimental_sql_impl_table_func(&vars::ENABLE_DATE_BIN_HOPPING, "
                    SELECT *
                    FROM pg_catalog.generate_series(
                        pg_catalog.date_bin($1, $3 + $1, '1970-01-01') - $2, $3, $1
                    ) AS dbh(date_bin_hopping)
                ") => ReturnType::set_of(Timestamp.into()), oid::FUNC_MZ_DATE_BIN_HOPPING_UNIX_EPOCH_TS_OID;
            // (hop, width, timestamp)
            params!(Interval, Interval, TimestampTz) => experimental_sql_impl_table_func(&vars::ENABLE_DATE_BIN_HOPPING, "
                    SELECT *
                    FROM pg_catalog.generate_series(
                        pg_catalog.date_bin($1, $3 + $1, '1970-01-01') - $2, $3, $1
                    ) AS dbh(date_bin_hopping)
                ") => ReturnType::set_of(TimestampTz.into()), oid::FUNC_MZ_DATE_BIN_HOPPING_UNIX_EPOCH_TSTZ_OID;
            // (hop, width, timestamp, origin)
            params!(Interval, Interval, Timestamp, Timestamp) => experimental_sql_impl_table_func(&vars::ENABLE_DATE_BIN_HOPPING, "
                    SELECT *
                    FROM pg_catalog.generate_series(
                        pg_catalog.date_bin($1, $3 + $1, $4) - $2, $3, $1
                    ) AS dbh(date_bin_hopping)
                ") => ReturnType::set_of(Timestamp.into()), oid::FUNC_MZ_DATE_BIN_HOPPING_TS_OID;
            // (hop, width, timestamp, origin)
            params!(Interval, Interval, TimestampTz, TimestampTz) => experimental_sql_impl_table_func(&vars::ENABLE_DATE_BIN_HOPPING, "
                    SELECT *
                    FROM pg_catalog.generate_series(
                        pg_catalog.date_bin($1, $3 + $1, $4) - $2, $3, $1
                    ) AS dbh(date_bin_hopping)
                ") => ReturnType::set_of(TimestampTz.into()), oid::FUNC_MZ_DATE_BIN_HOPPING_TSTZ_OID;
        },
        "encode" => Scalar {
            params!(Bytes, String) => BinaryFunc::Encode => String, 1946;
        },
        "decode" => Scalar {
            params!(String, String) => BinaryFunc::Decode => Bytes, 1947;
        },
        "regexp_split_to_array" => Scalar {
            params!(String, String) => VariadicFunc::RegexpSplitToArray => ScalarType::Array(Box::new(ScalarType::String)), 2767;
            params!(String, String, String) => VariadicFunc::RegexpSplitToArray => ScalarType::Array(Box::new(ScalarType::String)), 2768;
        },
        "regexp_split_to_table" => Table {
            params!(String, String) => sql_impl_table_func("
                SELECT unnest(regexp_split_to_array($1, $2))
            ") => ReturnType::set_of(String.into()), 2765;
            params!(String, String, String) => sql_impl_table_func("
                SELECT unnest(regexp_split_to_array($1, $2, $3))
            ") => ReturnType::set_of(String.into()), 2766;
        },
        "regexp_replace" => Scalar {
            params!(String, String, String) => VariadicFunc::RegexpReplace => String, 2284;
            params!(String, String, String, String) => VariadicFunc::RegexpReplace => String, 2285;
            // TODO: PostgreSQL supports additional five and six argument forms of this function which
            // allow controlling where to start the replacement and how many replacements to make.
        },
        "regexp_matches" => Table {
            params!(String, String) => Operation::variadic(move |_ecx, exprs| {
                let column_names = vec!["regexp_matches".into()];
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::RegexpMatches,
                        exprs: vec![exprs[0].clone(), exprs[1].clone()],
                    },
                    column_names,
                })
            }) => ReturnType::set_of(ScalarType::Array(Box::new(ScalarType::String)).into()), 2763;
            params!(String, String, String) => Operation::variadic(move |_ecx, exprs| {
                let column_names = vec!["regexp_matches".into()];
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::RegexpMatches,
                        exprs: vec![exprs[0].clone(), exprs[1].clone(), exprs[2].clone()],
                    },
                    column_names,
                })
            }) => ReturnType::set_of(ScalarType::Array(Box::new(ScalarType::String)).into()), 2764;
        },
        "reverse" => Scalar {
            params!(String) => UnaryFunc::Reverse(func::Reverse) => String, 3062;
        }
    };

    // Add side-effecting functions, which are defined in a separate module
    // using a restricted set of function definition features (e.g., no
    // overloads) to make them easier to plan.
    for sef_builtin in PG_CATALOG_SEF_BUILTINS.values() {
        builtins.insert(
            sef_builtin.name,
            Func::Scalar(vec![FuncImpl {
                oid: sef_builtin.oid,
                params: ParamList::Exact(
                    sef_builtin
                        .param_types
                        .iter()
                        .map(|t| ParamType::from(t.clone()))
                        .collect(),
                ),
                return_type: ReturnType::scalar(ParamType::from(
                    sef_builtin.return_type.scalar_type.clone(),
                )),
                op: Operation::variadic(|_ecx, _e| {
                    bail_unsupported!(format!("{} in this position", sef_builtin.name))
                }),
            }]),
        );
    }

    builtins
});

pub static INFORMATION_SCHEMA_BUILTINS: LazyLock<BTreeMap<&'static str, Func>> =
    LazyLock::new(|| {
        use ParamType::*;
        builtins! {
            "_pg_expandarray" => Table {
                // See: https://github.com/postgres/postgres/blob/16e3ad5d143795b05a21dc887c2ab384cce4bcb8/src/backend/catalog/information_schema.sql#L43
                params!(ArrayAny) => sql_impl_table_func("
                    SELECT
                        $1[s] AS x,
                        s - pg_catalog.array_lower($1, 1) + 1 AS n
                    FROM pg_catalog.generate_series(
                        pg_catalog.array_lower($1, 1),
                        pg_catalog.array_upper($1, 1),
                        1) as g(s)
                ") => ReturnType::set_of(RecordAny), oid::FUNC_PG_EXPAND_ARRAY;
            }
        }
    });

pub static MZ_CATALOG_BUILTINS: LazyLock<BTreeMap<&'static str, Func>> = LazyLock::new(|| {
    use ParamType::*;
    use ScalarBaseType::*;
    builtins! {
        "constant_time_eq" => Scalar {
            params!(Bytes, Bytes) => BinaryFunc::ConstantTimeEqBytes => Bool, oid::FUNC_CONSTANT_TIME_EQ_BYTES_OID;
            params!(String, String) => BinaryFunc::ConstantTimeEqString => Bool, oid::FUNC_CONSTANT_TIME_EQ_STRING_OID;
        },
        // Note: this is the original version of the AVG(...) function, as it existed prior to
        // v0.66. We updated the internal type promotion used when summing values to increase
        // precision, but objects (e.g. materialized views) that already used the AVG(...) function
        // could not be changed. So we migrated all existing uses of the AVG(...) function to this
        // version.
        //
        // TODO(parkmycar): When objects no longer depend on this function we can safely delete it.
        "avg_internal_v1" => Scalar {
            params!(Int64) => Operation::nullary(|_ecx| catalog_name_only!("avg_internal_v1")) => Numeric, oid::FUNC_AVG_INTERNAL_V1_INT64_OID;
            params!(Int32) => Operation::nullary(|_ecx| catalog_name_only!("avg_internal_v1")) => Numeric, oid::FUNC_AVG_INTERNAL_V1_INT32_OID;
            params!(Int16) => Operation::nullary(|_ecx| catalog_name_only!("avg_internal_v1")) => Numeric, oid::FUNC_AVG_INTERNAL_V1_INT16_OID;
            params!(UInt64) => Operation::nullary(|_ecx| catalog_name_only!("avg_internal_v1")) => Numeric, oid::FUNC_AVG_INTERNAL_V1_UINT64_OID;
            params!(UInt32) => Operation::nullary(|_ecx| catalog_name_only!("avg_internal_v1")) => Numeric, oid::FUNC_AVG_INTERNAL_V1_UINT32_OID;
            params!(UInt16) => Operation::nullary(|_ecx| catalog_name_only!("avg_internal_v1")) => Numeric, oid::FUNC_AVG_INTERNAL_V1_UINT16_OID;
            params!(Float32) => Operation::nullary(|_ecx| catalog_name_only!("avg_internal_v1")) => Float64, oid::FUNC_AVG_INTERNAL_V1_FLOAT32_OID;
            params!(Float64) => Operation::nullary(|_ecx| catalog_name_only!("avg_internal_v1")) => Float64, oid::FUNC_AVG_INTERNAL_V1_FLOAT64_OID;
            params!(Interval) => Operation::nullary(|_ecx| catalog_name_only!("avg_internal_v1")) => Interval, oid::FUNC_AVG_INTERNAL_V1_INTERVAL_OID;
        },
        "csv_extract" => Table {
            params!(Int64, String) => Operation::binary(move |_ecx, ncols, input| {
                const MAX_EXTRACT_COLUMNS: i64 = 8192;
                const TOO_MANY_EXTRACT_COLUMNS: i64 = MAX_EXTRACT_COLUMNS + 1;

                let ncols = match ncols.into_literal_int64() {
                    None | Some(i64::MIN..=0) => {
                        sql_bail!("csv_extract number of columns must be a positive integer literal");
                    },
                    Some(ncols @ 1..=MAX_EXTRACT_COLUMNS) => ncols,
                    Some(ncols @ TOO_MANY_EXTRACT_COLUMNS..) => {
                        return Err(PlanError::TooManyColumns {
                            max_num_columns: usize::try_from(MAX_EXTRACT_COLUMNS).unwrap_or(usize::MAX),
                            req_num_columns: usize::try_from(ncols).unwrap_or(usize::MAX),
                        });
                    },
                };
                let ncols = usize::try_from(ncols).expect("known to be greater than zero");

                let column_names = (1..=ncols).map(|i| format!("column{}", i).into()).collect();
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::CsvExtract(ncols),
                        exprs: vec![input],
                    },
                    column_names,
                })
            }) => ReturnType::set_of(RecordAny), oid::FUNC_CSV_EXTRACT_OID;
        },
        "concat_agg" => Aggregate {
            params!(Any) => Operation::unary(|_ecx, _e| bail_unsupported!("concat_agg")) => String, oid::FUNC_CONCAT_AGG_OID;
        },
        "crc32" => Scalar {
            params!(String) => UnaryFunc::Crc32String(func::Crc32String) => UInt32, oid::FUNC_CRC32_STRING_OID;
            params!(Bytes) => UnaryFunc::Crc32Bytes(func::Crc32Bytes) => UInt32, oid::FUNC_CRC32_BYTES_OID;
        },
        "datediff" => Scalar {
            params!(String, Timestamp, Timestamp) => VariadicFunc::DateDiffTimestamp => Int64, oid::FUNC_DATEDIFF_TIMESTAMP;
            params!(String, TimestampTz, TimestampTz) => VariadicFunc::DateDiffTimestampTz => Int64, oid::FUNC_DATEDIFF_TIMESTAMPTZ;
            params!(String, Date, Date) => VariadicFunc::DateDiffDate => Int64, oid::FUNC_DATEDIFF_DATE;
            params!(String, Time, Time) => VariadicFunc::DateDiffTime => Int64, oid::FUNC_DATEDIFF_TIME;
        },
        // We can't use the `privilege_fn!` macro because the macro relies on the object having an
        // OID, and clusters do not have OIDs.
        "has_cluster_privilege" => Scalar {
            params!(String, String, String) => sql_impl_func("has_cluster_privilege(mz_internal.mz_role_oid($1), $2, $3)") => Bool, oid::FUNC_HAS_CLUSTER_PRIVILEGE_TEXT_TEXT_TEXT_OID;
            params!(Oid, String, String) => sql_impl_func(&format!("
                CASE
                -- We must first check $2 to avoid a potentially null error message (an error itself).
                WHEN $2 IS NULL
                THEN NULL
                -- Validate the cluster name in order to return a proper error.
                WHEN NOT EXISTS (SELECT name FROM mz_clusters WHERE name = $2)
                THEN mz_unsafe.mz_error_if_null(NULL::boolean, 'error cluster \"' || $2 || '\" does not exist')
                -- Validate the privileges and other arguments.
                WHEN NOT mz_internal.mz_validate_privileges($3)
                OR $1 IS NULL
                OR $3 IS NULL
                OR $1 NOT IN (SELECT oid FROM mz_catalog.mz_roles)
                THEN NULL
                ELSE COALESCE(
                    (
                        SELECT
                            bool_or(
                                mz_internal.mz_acl_item_contains_privilege(privilege, $3)
                            )
                                AS has_cluster_privilege
                        FROM
                            (
                                SELECT
                                    unnest(privileges)
                                FROM
                                    mz_clusters
                                WHERE
                                    mz_clusters.name = $2
                            )
                                AS user_privs (privilege)
                            LEFT JOIN mz_catalog.mz_roles ON
                                    mz_internal.mz_aclitem_grantee(privilege) = mz_roles.id
                        WHERE
                            mz_internal.mz_aclitem_grantee(privilege) = '{}' OR pg_has_role($1, mz_roles.oid, 'USAGE')
                    ),
                    false
                )
                END
            ", RoleId::Public)) => Bool, oid::FUNC_HAS_CLUSTER_PRIVILEGE_OID_TEXT_TEXT_OID;
            params!(String, String) => sql_impl_func("has_cluster_privilege(current_user, $1, $2)") => Bool, oid::FUNC_HAS_CLUSTER_PRIVILEGE_TEXT_TEXT_OID;
        },
        "has_connection_privilege" => Scalar {
            params!(String, String, String) => sql_impl_func("has_connection_privilege(mz_internal.mz_role_oid($1), mz_internal.mz_connection_oid($2), $3)") => Bool, oid::FUNC_HAS_CONNECTION_PRIVILEGE_TEXT_TEXT_TEXT_OID;
            params!(String, Oid, String) => sql_impl_func("has_connection_privilege(mz_internal.mz_role_oid($1), $2, $3)") => Bool, oid::FUNC_HAS_CONNECTION_PRIVILEGE_TEXT_OID_TEXT_OID;
            params!(Oid, String, String) => sql_impl_func("has_connection_privilege($1, mz_internal.mz_connection_oid($2), $3)") => Bool, oid::FUNC_HAS_CONNECTION_PRIVILEGE_OID_TEXT_TEXT_OID;
            params!(Oid, Oid, String) => sql_impl_func(&privilege_fn!("has_connection_privilege", "mz_connections")) => Bool, oid::FUNC_HAS_CONNECTION_PRIVILEGE_OID_OID_TEXT_OID;
            params!(String, String) => sql_impl_func("has_connection_privilege(current_user, $1, $2)") => Bool, oid::FUNC_HAS_CONNECTION_PRIVILEGE_TEXT_TEXT_OID;
            params!(Oid, String) => sql_impl_func("has_connection_privilege(current_user, $1, $2)") => Bool, oid::FUNC_HAS_CONNECTION_PRIVILEGE_OID_TEXT_OID;
        },
        "has_role" => Scalar {
            params!(String, String, String) => sql_impl_func("pg_has_role($1, $2, $3)") => Bool, oid::FUNC_HAS_ROLE_TEXT_TEXT_TEXT_OID;
            params!(String, Oid, String) => sql_impl_func("pg_has_role($1, $2, $3)") => Bool, oid::FUNC_HAS_ROLE_TEXT_OID_TEXT_OID;
            params!(Oid, String, String) => sql_impl_func("pg_has_role($1, $2, $3)") => Bool, oid::FUNC_HAS_ROLE_OID_TEXT_TEXT_OID;
            params!(Oid, Oid, String) => sql_impl_func("pg_has_role($1, $2, $3)") => Bool, oid::FUNC_HAS_ROLE_OID_OID_TEXT_OID;
            params!(String, String) => sql_impl_func("pg_has_role($1, $2)") => Bool, oid::FUNC_HAS_ROLE_TEXT_TEXT_OID;
            params!(Oid, String) => sql_impl_func("pg_has_role($1, $2)") => Bool, oid::FUNC_HAS_ROLE_OID_TEXT_OID;
        },
        "has_secret_privilege" => Scalar {
            params!(String, String, String) => sql_impl_func("has_secret_privilege(mz_internal.mz_role_oid($1), mz_internal.mz_secret_oid($2), $3)") => Bool, oid::FUNC_HAS_SECRET_PRIVILEGE_TEXT_TEXT_TEXT_OID;
            params!(String, Oid, String) => sql_impl_func("has_secret_privilege(mz_internal.mz_role_oid($1), $2, $3)") => Bool, oid::FUNC_HAS_SECRET_PRIVILEGE_TEXT_OID_TEXT_OID;
            params!(Oid, String, String) => sql_impl_func("has_secret_privilege($1, mz_internal.mz_secret_oid($2), $3)") => Bool, oid::FUNC_HAS_SECRET_PRIVILEGE_OID_TEXT_TEXT_OID;
            params!(Oid, Oid, String) => sql_impl_func(&privilege_fn!("has_secret_privilege", "mz_secrets")) => Bool, oid::FUNC_HAS_SECRET_PRIVILEGE_OID_OID_TEXT_OID;
            params!(String, String) => sql_impl_func("has_secret_privilege(current_user, $1, $2)") => Bool, oid::FUNC_HAS_SECRET_PRIVILEGE_TEXT_TEXT_OID;
            params!(Oid, String) => sql_impl_func("has_secret_privilege(current_user, $1, $2)") => Bool, oid::FUNC_HAS_SECRET_PRIVILEGE_OID_TEXT_OID;
        },
        "has_system_privilege" => Scalar {
            params!(String, String) => sql_impl_func("has_system_privilege(mz_internal.mz_role_oid($1), $2)") => Bool, oid::FUNC_HAS_SYSTEM_PRIVILEGE_TEXT_TEXT_OID;
            params!(Oid, String) => sql_impl_func(&format!("
                CASE
                -- We need to validate the privileges to return a proper error before
                -- anything else.
                WHEN NOT mz_internal.mz_validate_privileges($2)
                OR $1 IS NULL
                OR $2 IS NULL
                OR $1 NOT IN (SELECT oid FROM mz_catalog.mz_roles)
                THEN NULL
                ELSE COALESCE(
                    (
                        SELECT
                            bool_or(
                                mz_internal.mz_acl_item_contains_privilege(privileges, $2)
                            )
                                AS has_system_privilege
                        FROM mz_catalog.mz_system_privileges
                        LEFT JOIN mz_catalog.mz_roles ON
                                mz_internal.mz_aclitem_grantee(privileges) = mz_roles.id
                        WHERE
                            mz_internal.mz_aclitem_grantee(privileges) = '{}' OR pg_has_role($1, mz_roles.oid, 'USAGE')
                    ),
                    false
                )
                END
            ", RoleId::Public)) => Bool, oid::FUNC_HAS_SYSTEM_PRIVILEGE_OID_TEXT_OID;
            params!(String) => sql_impl_func("has_system_privilege(current_user, $1)") => Bool, oid::FUNC_HAS_SYSTEM_PRIVILEGE_TEXT_OID;
        },
        "has_type_privilege" => Scalar {
            params!(String, String, String) => sql_impl_func("has_type_privilege(mz_internal.mz_role_oid($1), $2::regtype::oid, $3)") => Bool, 3138;
            params!(String, Oid, String) => sql_impl_func("has_type_privilege(mz_internal.mz_role_oid($1), $2, $3)") => Bool, 3139;
            params!(Oid, String, String) => sql_impl_func("has_type_privilege($1, $2::regtype::oid, $3)") => Bool, 3140;
            params!(Oid, Oid, String) => sql_impl_func(&privilege_fn!("has_type_privilege", "mz_types")) => Bool, 3141;
            params!(String, String) => sql_impl_func("has_type_privilege(current_user, $1, $2)") => Bool, 3142;
            params!(Oid, String) => sql_impl_func("has_type_privilege(current_user, $1, $2)") => Bool, 3143;
        },
        "kafka_murmur2" => Scalar {
            params!(String) => UnaryFunc::KafkaMurmur2String(func::KafkaMurmur2String) => Int32, oid::FUNC_KAFKA_MURMUR2_STRING_OID;
            params!(Bytes) => UnaryFunc::KafkaMurmur2Bytes(func::KafkaMurmur2Bytes) => Int32, oid::FUNC_KAFKA_MURMUR2_BYTES_OID;
        },
        "list_agg" => Aggregate {
            params!(Any) => Operation::unary_ordered(|ecx, e, order_by| {
                if let ScalarType::Char {.. }  = ecx.scalar_type(&e) {
                    bail_unsupported!("list_agg on char");
                };
                // ListConcat excepts all inputs to be lists, so wrap all input datums into
                // lists.
                let e_arr = HirScalarExpr::call_variadic(
                    VariadicFunc::ListCreate { elem_type: ecx.scalar_type(&e) },
                    vec![e],
                );
                Ok((e_arr, AggregateFunc::ListConcat { order_by }))
            }) => ListAnyCompatible,  oid::FUNC_LIST_AGG_OID;
        },
        "list_append" => Scalar {
            vec![ListAnyCompatible, ListElementAnyCompatible] => BinaryFunc::ListElementConcat => ListAnyCompatible, oid::FUNC_LIST_APPEND_OID;
        },
        "list_cat" => Scalar {
            vec![ListAnyCompatible, ListAnyCompatible] => BinaryFunc::ListListConcat => ListAnyCompatible, oid::FUNC_LIST_CAT_OID;
        },
        "list_n_layers" => Scalar {
            vec![ListAny] => Operation::unary(|ecx, e| {
                ecx.require_feature_flag(&crate::session::vars::ENABLE_LIST_N_LAYERS)?;
                let d = ecx.scalar_type(&e).unwrap_list_n_layers();
                match i32::try_from(d) {
                    Ok(d) => Ok(HirScalarExpr::literal(Datum::Int32(d), ScalarType::Int32)),
                    Err(_) => sql_bail!("list has more than {} layers", i32::MAX),
                }

            }) => Int32, oid::FUNC_LIST_N_LAYERS_OID;
        },
        "list_length" => Scalar {
            vec![ListAny] => UnaryFunc::ListLength(func::ListLength) => Int32, oid::FUNC_LIST_LENGTH_OID;
        },
        "list_length_max" => Scalar {
            vec![ListAny, Plain(ScalarType::Int64)] => Operation::binary(|ecx, lhs, rhs| {
                ecx.require_feature_flag(&crate::session::vars::ENABLE_LIST_LENGTH_MAX)?;
                let max_layer = ecx.scalar_type(&lhs).unwrap_list_n_layers();
                Ok(lhs.call_binary(rhs, BinaryFunc::ListLengthMax { max_layer }))
            }) => Int32, oid::FUNC_LIST_LENGTH_MAX_OID;
        },
        "list_prepend" => Scalar {
            vec![ListElementAnyCompatible, ListAnyCompatible] => BinaryFunc::ElementListConcat => ListAnyCompatible, oid::FUNC_LIST_PREPEND_OID;
        },
        "list_remove" => Scalar {
            vec![ListAnyCompatible, ListElementAnyCompatible] => Operation::binary(|ecx, lhs, rhs| {
                ecx.require_feature_flag(&crate::session::vars::ENABLE_LIST_REMOVE)?;
                Ok(lhs.call_binary(rhs, BinaryFunc::ListRemove))
            }) => ListAnyCompatible, oid::FUNC_LIST_REMOVE_OID;
        },
        "map_agg" => Aggregate {
            params!(String, Any) => Operation::binary_ordered(|ecx, key, val, order_by| {
                let (value_type, val) = match ecx.scalar_type(&val) {
                    // TODO(see <materialize#7572>): remove this
                    ScalarType::Char { length } => (ScalarType::Char { length }, val.call_unary(UnaryFunc::PadChar(func::PadChar { length }))),
                    typ => (typ, val),
                };

                let e = HirScalarExpr::call_variadic(
                    VariadicFunc::RecordCreate {
                        field_names: vec![ColumnName::from("key"), ColumnName::from("val")],
                    },
                    vec![key, val],
                );

                Ok((e, AggregateFunc::MapAgg { order_by, value_type }))
            }) => MapAny, oid::FUNC_MAP_AGG;
        },
        "map_build" => Scalar {
            // TODO: support a function to construct maps that looks like...
            //
            // params!([String], Any...) => Operation::variadic(|ecx, exprs| {
            //
            // ...the challenge here is that we don't support constructing other
            // complex types from varidaic functions and instead use a SQL
            // keyword; however that doesn't work very well for map because the
            // intuitive syntax would be something akin to `MAP[key=>value]`,
            // but that doesn't work out of the box because `key=>value` looks
            // like an expression.
            params!(ListAny) => Operation::unary(|ecx, expr| {
                let ty = ecx.scalar_type(&expr);

                // This is a fake error but should suffice given how exotic the
                // function is.
                let err = || {
                    Err(sql_err!(
                        "function map_build({}) does not exist",
                        ecx.humanize_scalar_type(&ty.clone(), false)
                    ))
                };

                // This function only accepts lists of records whose schema is
                // (text, T).
                let value_type = match &ty {
                    ScalarType::List { element_type, .. } => match &**element_type {
                        ScalarType::Record { fields, .. } if fields.len() == 2 => {
                            if fields[0].1.scalar_type != ScalarType::String {
                                return err();
                            }

                            fields[1].1.scalar_type.clone()
                        }
                        _ => return err(),
                    },
                    _ => unreachable!("input guaranteed to be list"),
                };

                Ok(expr.call_unary(UnaryFunc::MapBuildFromRecordList(
                    func::MapBuildFromRecordList { value_type },
                )))
            }) => MapAny, oid::FUNC_MAP_BUILD;
        },
        "map_length" => Scalar {
            params![MapAny] => UnaryFunc::MapLength(func::MapLength) => Int32, oid::FUNC_MAP_LENGTH_OID;
        },
        "mz_environment_id" => Scalar {
            params!() => UnmaterializableFunc::MzEnvironmentId => String, oid::FUNC_MZ_ENVIRONMENT_ID_OID;
        },
        "mz_is_superuser" => Scalar {
            params!() => UnmaterializableFunc::MzIsSuperuser => ScalarType::Bool, oid::FUNC_MZ_IS_SUPERUSER;
        },
        "mz_logical_timestamp" => Scalar {
            params!() => Operation::nullary(|_ecx| sql_bail!("mz_logical_timestamp() has been renamed to mz_now()")) => MzTimestamp, oid::FUNC_MZ_LOGICAL_TIMESTAMP_OID;
        },
        "mz_now" => Scalar {
            params!() => UnmaterializableFunc::MzNow => MzTimestamp, oid::FUNC_MZ_NOW_OID;
        },
        "mz_uptime" => Scalar {
            params!() => UnmaterializableFunc::MzUptime => Interval, oid::FUNC_MZ_UPTIME_OID;
        },
        "mz_version" => Scalar {
            params!() => UnmaterializableFunc::MzVersion => String, oid::FUNC_MZ_VERSION_OID;
        },
        "mz_version_num" => Scalar {
            params!() => UnmaterializableFunc::MzVersionNum => Int32, oid::FUNC_MZ_VERSION_NUM_OID;
        },
        "pretty_sql" => Scalar {
            params!(String, Int32) => BinaryFunc::PrettySql => String, oid::FUNC_PRETTY_SQL;
            params!(String) => Operation::unary(|_ecx, s| {
                let width = HirScalarExpr::literal(Datum::Int32(mz_sql_pretty::DEFAULT_WIDTH.try_into().expect("must fit")), ScalarType::Int32);
                Ok(s.call_binary(width, BinaryFunc::PrettySql))
            }) => String, oid::FUNC_PRETTY_SQL_NOWIDTH;
        },
        "regexp_extract" => Table {
            params!(String, String) => Operation::binary(move |_ecx, regex, haystack| {
                let regex = match regex.into_literal_string() {
                    None => sql_bail!("regexp_extract requires a string literal as its first argument"),
                    Some(regex) => mz_expr::AnalyzedRegex::new(&regex, mz_expr::AnalyzedRegexOpts::default()).map_err(|e| sql_err!("analyzing regex: {}", e))?,
                };
                let column_names = regex
                    .capture_groups_iter()
                    .map(|cg| {
                        cg.name.clone().unwrap_or_else(|| format!("column{}", cg.index)).into()
                    })
                    .collect::<Vec<_>>();
                if column_names.is_empty(){
                    sql_bail!("regexp_extract must specify at least one capture group");
                }
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::RegexpExtract(regex),
                        exprs: vec![haystack],
                    },
                    column_names,
                })
            }) => ReturnType::set_of(RecordAny), oid::FUNC_REGEXP_EXTRACT_OID;
        },
        "repeat_row" => Table {
            params!(Int64) => Operation::unary(move |ecx, n| {
                ecx.require_feature_flag(&crate::session::vars::ENABLE_REPEAT_ROW)?;
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::Repeat,
                        exprs: vec![n],
                    },
                    column_names: vec![]
                })
            }) => ReturnType::none(true), oid::FUNC_REPEAT_OID;
        },
        "seahash" => Scalar {
            params!(String) => UnaryFunc::SeahashString(func::SeahashString) => UInt32, oid::FUNC_SEAHASH_STRING_OID;
            params!(Bytes) => UnaryFunc::SeahashBytes(func::SeahashBytes) => UInt32, oid::FUNC_SEAHASH_BYTES_OID;
        },
        "starts_with" => Scalar {
            params!(String, String) => BinaryFunc::StartsWith => Bool, 3696;
        },
        "timezone_offset" => Scalar {
            params!(String, TimestampTz) => BinaryFunc::TimezoneOffset => RecordAny, oid::FUNC_TIMEZONE_OFFSET;
        },
        "try_parse_monotonic_iso8601_timestamp" => Scalar {
            params!(String) => Operation::unary(move |_ecx, e| {
                Ok(e.call_unary(UnaryFunc::TryParseMonotonicIso8601Timestamp(func::TryParseMonotonicIso8601Timestamp)))
            }) => Timestamp, oid::FUNC_TRY_PARSE_MONOTONIC_ISO8601_TIMESTAMP;
        },
        "unnest" => Table {
            vec![ArrayAny] => Operation::unary(move |ecx, e| {
                let el_typ = ecx.scalar_type(&e).unwrap_array_element_type().clone();
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::UnnestArray { el_typ },
                        exprs: vec![e],
                    },
                    column_names: vec!["unnest".into()],
                })
            }) =>
                // This return type should be equivalent to "ArrayElementAny", but this would be its sole use.
                ReturnType::set_of(AnyElement), 2331;
            vec![ListAny] => Operation::unary(move |ecx, e| {
                let el_typ = ecx.scalar_type(&e).unwrap_list_element_type().clone();
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::UnnestList { el_typ },
                        exprs: vec![e],
                    },
                    column_names: vec!["unnest".into()],
                })
            }) =>
                // This return type should be equivalent to "ListElementAny", but this would be its sole use.
                ReturnType::set_of(Any), oid::FUNC_UNNEST_LIST_OID;
            vec![MapAny] => Operation::unary(move |ecx, e| {
                let value_type = ecx.scalar_type(&e).unwrap_map_value_type().clone();
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::UnnestMap { value_type },
                        exprs: vec![e],
                    },
                    column_names: vec!["key".into(), "value".into()],
                })
            }) =>
                // This return type should be equivalent to "ListElementAny", but this would be its sole use.
                ReturnType::set_of(Any), oid::FUNC_UNNEST_MAP_OID;
        }
    }
});

pub static MZ_INTERNAL_BUILTINS: LazyLock<BTreeMap<&'static str, Func>> = LazyLock::new(|| {
    use ParamType::*;
    use ScalarBaseType::*;
    builtins! {
        "aclitem_grantor" => Scalar {
            params!(AclItem) => UnaryFunc::AclItemGrantor(func::AclItemGrantor) => Oid, oid::FUNC_ACL_ITEM_GRANTOR_OID;
        },
        "aclitem_grantee" => Scalar {
            params!(AclItem) => UnaryFunc::AclItemGrantee(func::AclItemGrantee) => Oid, oid::FUNC_ACL_ITEM_GRANTEE_OID;
        },
        "aclitem_privileges" => Scalar {
            params!(AclItem) => UnaryFunc::AclItemPrivileges(func::AclItemPrivileges) => String, oid::FUNC_ACL_ITEM_PRIVILEGES_OID;
        },
        "is_rbac_enabled" => Scalar {
            params!() => UnmaterializableFunc::IsRbacEnabled => Bool, oid::FUNC_IS_RBAC_ENABLED_OID;
        },
        "make_mz_aclitem" => Scalar {
            params!(String, String, String) => VariadicFunc::MakeMzAclItem => MzAclItem, oid::FUNC_MAKE_MZ_ACL_ITEM_OID;
        },
        "mz_acl_item_contains_privilege" => Scalar {
            params!(MzAclItem, String) => BinaryFunc::MzAclItemContainsPrivilege => Bool, oid::FUNC_MZ_ACL_ITEM_CONTAINS_PRIVILEGE_OID;
        },
        "mz_aclexplode" => Table {
            params!(ScalarType::Array(Box::new(ScalarType::MzAclItem))) =>  Operation::unary(move |_ecx, mz_aclitems| {
                Ok(TableFuncPlan {
                    expr: HirRelationExpr::CallTable {
                        func: TableFunc::MzAclExplode,
                        exprs: vec![mz_aclitems],
                    },
                    column_names: vec!["grantor".into(), "grantee".into(), "privilege_type".into(), "is_grantable".into()],
                })
            }) => ReturnType::set_of(RecordAny), oid::FUNC_MZ_ACL_ITEM_EXPLODE_OID;
        },
        "mz_aclitem_grantor" => Scalar {
            params!(MzAclItem) => UnaryFunc::MzAclItemGrantor(func::MzAclItemGrantor) => String, oid::FUNC_MZ_ACL_ITEM_GRANTOR_OID;
        },
        "mz_aclitem_grantee" => Scalar {
            params!(MzAclItem) => UnaryFunc::MzAclItemGrantee(func::MzAclItemGrantee) => String, oid::FUNC_MZ_ACL_ITEM_GRANTEE_OID;
        },
        "mz_aclitem_privileges" => Scalar {
            params!(MzAclItem) => UnaryFunc::MzAclItemPrivileges(func::MzAclItemPrivileges) => String, oid::FUNC_MZ_ACL_ITEM_PRIVILEGES_OID;
        },
        // There is no regclass equivalent for roles to look up connections, so we
        // have this helper function instead.
        //
        // TODO: invent an OID alias for connections
        "mz_connection_oid" => Scalar {
            params!(String) => sql_impl_func("
                CASE
                WHEN $1 IS NULL THEN NULL
                ELSE (
                    mz_unsafe.mz_error_if_null(
                        (SELECT oid FROM mz_catalog.mz_objects WHERE name = $1 AND type = 'connection'),
                        'connection \"' || $1 || '\" does not exist'
                    )
                )
                END
            ") => Oid, oid::FUNC_CONNECTION_OID_OID;
        },
        "mz_format_privileges" => Scalar {
            params!(String) => UnaryFunc::MzFormatPrivileges(func::MzFormatPrivileges) => ScalarType::Array(Box::new(ScalarType::String)), oid::FUNC_MZ_FORMAT_PRIVILEGES_OID;
        },
        "mz_name_rank" => Table {
            // Determines the id, rank of all objects that can be matched using
            // the provided args.
            params!(
                // Database
                String,
                // Schemas/search path
                ParamType::Plain(ScalarType::Array(Box::new(ScalarType::String))),
                // Item name
                String,
                // Get rank among particular OID alias (e.g. regclass)
                String
            ) =>
            // credit for using rank() to @def-
            sql_impl_table_func("
            -- The best ranked name is the one that belongs to the schema correlated with the lowest
            -- index in the search path
            SELECT id, name, count, min(schema_pref) OVER () = schema_pref AS best_ranked FROM (
                SELECT DISTINCT
                    o.id,
                    ARRAY[CASE WHEN s.database_id IS NULL THEN NULL ELSE d.name END, s.name, o.name]
                    AS name,
                    o.count,
                    pg_catalog.array_position($2, s.name) AS schema_pref
                FROM
                    (
                        SELECT
                            o.id,
                            o.schema_id,
                            o.name,
                            count(*)
                        FROM mz_catalog.mz_objects AS o
                        JOIN mz_internal.mz_object_oid_alias AS a
                            ON o.type = a.object_type
                        WHERE o.name = CAST($3 AS pg_catalog.text) AND a.oid_alias = $4
                        GROUP BY 1, 2, 3
                    )
                        AS o
                    JOIN mz_catalog.mz_schemas AS s ON o.schema_id = s.id
                    JOIN
                        unnest($2) AS search_schema (name)
                        ON search_schema.name = s.name
                    JOIN
                        (
                            SELECT id, name FROM mz_catalog.mz_databases
                            -- If the provided database does not exist, add a row for it so that it
                            -- can still join against ambient schemas.
                            UNION ALL
                            SELECT '', $1 WHERE $1 NOT IN (SELECT name FROM mz_catalog.mz_databases)
                        ) AS d
                        ON d.id = COALESCE(s.database_id, d.id)
                WHERE d.name = CAST($1 AS pg_catalog.text)
            );
            ") => ReturnType::set_of(RecordAny), oid::FUNC_MZ_NAME_RANK;
        },
        "mz_resolve_object_name" => Table {
            params!(String, String) =>
            // Normalize the input name, and for any NULL values (e.g. not database qualified), use
            // the defaults used during name resolution.
            sql_impl_table_func("
                SELECT
                    o.id, o.oid, o.schema_id, o.name, o.type, o.owner_id, o.privileges
                FROM
                    (SELECT mz_internal.mz_normalize_object_name($2))
                            AS normalized (n),
                    mz_internal.mz_name_rank(
                        COALESCE(n[1], pg_catalog.current_database()),
                        CASE
                            WHEN n[2] IS NULL
                                THEN pg_catalog.current_schemas(true)
                            ELSE
                                ARRAY[n[2]]
                        END,
                        n[3],
                        $1
                    ) AS r,
                    mz_catalog.mz_objects AS o
                WHERE r.id = o.id AND r.best_ranked;
            ") => ReturnType::set_of(RecordAny), oid::FUNC_MZ_RESOLVE_OBJECT_NAME;
        },
        // Returns the an array representing the minimal namespace a user must
        // provide to refer to an item whose name is the first argument.
        //
        // The first argument must be a fully qualified name (i.e. contain
        // database.schema.object), with each level of the namespace being an
        // element.
        //
        // The second argument represents the `GlobalId` of the resolved object.
        // This is a safeguard to ensure that the name we are resolving refers
        // to the expected entry. For example, this helps us disambiguate cases
        // where e.g. types and functions have the same name.
        "mz_minimal_name_qualification" => Scalar {
            params!(ScalarType::Array(Box::new(ScalarType::String)), String) => {
                sql_impl_func("(
                    SELECT
                    CASE
                        WHEN $1::pg_catalog.text[] IS NULL
                            THEN NULL
                    -- If DB doesn't match, requires full qual
                        WHEN $1[1] != pg_catalog.current_database()
                            THEN $1
                    -- If not in currently searchable schema, must be schema qualified
                        WHEN NOT $1[2] = ANY(pg_catalog.current_schemas(true))
                            THEN ARRAY[$1[2], $1[3]]
                    ELSE
                        minimal_name
                    END
                FROM (
                    -- Subquery so we return one null row in the cases where
                    -- there are no matches.
                    SELECT (
                        SELECT DISTINCT
                            CASE
                                -- If there is only one item with this name and it's rank 1,
                                -- it is uniquely nameable with just the final element
                                WHEN best_ranked AND count = 1
                                    THEN ARRAY[r.name[3]]
                                -- Otherwise, it is findable in the search path, so does not
                                -- need database qualification
                                ELSE
                                    ARRAY[r.name[2], r.name[3]]
                            END AS minimal_name
                        FROM mz_catalog.mz_objects AS o
                            JOIN mz_internal.mz_object_oid_alias AS a
                                ON o.type = a.object_type,
                            -- implied lateral to put the OID alias into scope
                            mz_internal.mz_name_rank(
                                pg_catalog.current_database(),
                                pg_catalog.current_schemas(true),
                                $1[3],
                                a.oid_alias
                            ) AS r
                        WHERE o.id = $2 AND r.id = $2
                    )
                )
            )")
            } => ScalarType::Array(Box::new(ScalarType::String)), oid::FUNC_MZ_MINIMINAL_NAME_QUALIFICATION;
        },
        "mz_global_id_to_name" => Scalar {
            params!(String) => sql_impl_func("
            CASE
                WHEN $1 IS NULL THEN NULL
                ELSE (
                    SELECT array_to_string(minimal_name, '.')
                    FROM (
                        SELECT mz_unsafe.mz_error_if_null(
                            (
                                -- Return the fully-qualified name
                                SELECT DISTINCT ARRAY[qual.d, qual.s, item.name]
                                FROM
                                    mz_catalog.mz_objects AS item
                                JOIN
                                (
                                    SELECT
                                        d.name AS d,
                                        s.name AS s,
                                        s.id AS schema_id
                                    FROM
                                        mz_catalog.mz_schemas AS s
                                        LEFT JOIN
                                            (SELECT id, name FROM mz_catalog.mz_databases)
                                            AS d
                                            ON s.database_id = d.id
                                ) AS qual
                                ON qual.schema_id = item.schema_id
                                WHERE item.id = CAST($1 AS text)
                            ),
                            'global ID ' || $1 || ' does not exist'
                        )
                    ) AS n (fqn),
                    LATERAL (
                        -- Get the minimal qualification of the fully qualified name
                        SELECT mz_internal.mz_minimal_name_qualification(fqn, $1)
                    ) AS m (minimal_name)
                )
                END
            ") => String, oid::FUNC_MZ_GLOBAL_ID_TO_NAME;
        },
        "mz_normalize_object_name" => Scalar {
            params!(String) => sql_impl_func("
            (
                SELECT
                    CASE
                        WHEN $1 IS NULL THEN NULL
                        WHEN pg_catalog.array_length(ident, 1) > 3
                            THEN mz_unsafe.mz_error_if_null(
                                NULL::pg_catalog.text[],
                                'improper relation name (too many dotted names): ' || $1
                            )
                        ELSE pg_catalog.array_cat(
                            pg_catalog.array_fill(
                                CAST(NULL AS pg_catalog.text),
                                ARRAY[3 - pg_catalog.array_length(ident, 1)]
                            ),
                            ident
                        )
                    END
                FROM (
                    SELECT pg_catalog.parse_ident($1) AS ident
                ) AS i
            )") => ScalarType::Array(Box::new(ScalarType::String)), oid::FUNC_MZ_NORMALIZE_OBJECT_NAME;
        },
        "mz_normalize_schema_name" => Scalar {
            params!(String) => sql_impl_func("
             (
                SELECT
                    CASE
                        WHEN $1 IS NULL THEN NULL
                        WHEN pg_catalog.array_length(ident, 1) > 2
                            THEN mz_unsafe.mz_error_if_null(
                                NULL::pg_catalog.text[],
                                'improper schema name (too many dotted names): ' || $1
                            )
                        ELSE pg_catalog.array_cat(
                            pg_catalog.array_fill(
                                CAST(NULL AS pg_catalog.text),
                                ARRAY[2 - pg_catalog.array_length(ident, 1)]
                            ),
                            ident
                        )
                    END
                FROM (
                    SELECT pg_catalog.parse_ident($1) AS ident
                ) AS i
            )") => ScalarType::Array(Box::new(ScalarType::String)), oid::FUNC_MZ_NORMALIZE_SCHEMA_NAME;
        },
        "mz_render_typmod" => Scalar {
            params!(Oid, Int32) => BinaryFunc::MzRenderTypmod => String, oid::FUNC_MZ_RENDER_TYPMOD_OID;
        },
        "mz_role_oid_memberships" => Scalar {
            params!() => UnmaterializableFunc::MzRoleOidMemberships => ScalarType::Map{ value_type: Box::new(ScalarType::Array(Box::new(ScalarType::String))), custom_id: None }, oid::FUNC_MZ_ROLE_OID_MEMBERSHIPS;
        },
        // There is no regclass equivalent for databases to look up oids, so we have this helper function instead.
        "mz_database_oid" => Scalar {
            params!(String) => sql_impl_func("
                CASE
                WHEN $1 IS NULL THEN NULL
                ELSE (
                    mz_unsafe.mz_error_if_null(
                        (SELECT oid FROM mz_databases WHERE name = $1),
                        'database \"' || $1 || '\" does not exist'
                    )
                )
                END
            ") => Oid, oid::FUNC_DATABASE_OID_OID;
        },
        // There is no regclass equivalent for schemas to look up oids, so we have this helper function instead.
        "mz_schema_oid" => Scalar {
            params!(String) => sql_impl_func("
            CASE
                WHEN $1 IS NULL THEN NULL
            ELSE
                mz_unsafe.mz_error_if_null(
                    (
                        SELECT
                            (
                                SELECT s.oid
                                FROM mz_catalog.mz_schemas AS s
                                LEFT JOIN mz_databases AS d ON s.database_id = d.id
                                WHERE
                                    (
                                        -- Filter to only schemas in the named database or the
                                        -- current database if no database was specified.
                                        d.name = COALESCE(n[1], pg_catalog.current_database())
                                        -- Always include all ambient schemas.
                                        OR s.database_id IS NULL
                                    )
                                    AND s.name = n[2]
                            )
                        FROM mz_internal.mz_normalize_schema_name($1) AS n
                    ),
                    'schema \"' || $1 || '\" does not exist'
                )
            END
            ") => Oid, oid::FUNC_SCHEMA_OID_OID;
        },
        // There is no regclass equivalent for roles to look up oids, so we have this helper function instead.
        "mz_role_oid" => Scalar {
            params!(String) => sql_impl_func("
                CASE
                WHEN $1 IS NULL THEN NULL
                ELSE (
                    mz_unsafe.mz_error_if_null(
                        (SELECT oid FROM mz_catalog.mz_roles WHERE name = $1),
                        'role \"' || $1 || '\" does not exist'
                    )
                )
                END
            ") => Oid, oid::FUNC_ROLE_OID_OID;
        },
        // There is no regclass equivalent for roles to look up secrets, so we
        // have this helper function instead.
        //
        // TODO: invent an OID alias for secrets
        "mz_secret_oid" => Scalar {
            params!(String) => sql_impl_func("
                CASE
                WHEN $1 IS NULL THEN NULL
                ELSE (
                    mz_unsafe.mz_error_if_null(
                        (SELECT oid FROM mz_catalog.mz_objects WHERE name = $1 AND type = 'secret'),
                        'secret \"' || $1 || '\" does not exist'
                    )
                )
                END
            ") => Oid, oid::FUNC_SECRET_OID_OID;
        },
        // This ought to be exposed in `mz_catalog`, but its name is rather
        // confusing. It does not identify the SQL session, but the
        // invocation of this `environmentd` process.
        "mz_session_id" => Scalar {
            params!() => UnmaterializableFunc::MzSessionId => Uuid, oid::FUNC_MZ_SESSION_ID_OID;
        },
        "mz_type_name" => Scalar {
            params!(Oid) => UnaryFunc::MzTypeName(func::MzTypeName) => String, oid::FUNC_MZ_TYPE_NAME;
        },
        "mz_validate_privileges" => Scalar {
            params!(String) => UnaryFunc::MzValidatePrivileges(func::MzValidatePrivileges) => Bool, oid::FUNC_MZ_VALIDATE_PRIVILEGES_OID;
        },
        "mz_validate_role_privilege" => Scalar {
            params!(String) => UnaryFunc::MzValidateRolePrivilege(func::MzValidateRolePrivilege) => Bool, oid::FUNC_MZ_VALIDATE_ROLE_PRIVILEGE_OID;
        }
    }
});

pub static MZ_UNSAFE_BUILTINS: LazyLock<BTreeMap<&'static str, Func>> = LazyLock::new(|| {
    use ParamType::*;
    use ScalarBaseType::*;
    builtins! {
        "mz_all" => Aggregate {
            params!(Any) => AggregateFunc::All => Bool, oid::FUNC_MZ_ALL_OID;
        },
        "mz_any" => Aggregate {
            params!(Any) => AggregateFunc::Any => Bool, oid::FUNC_MZ_ANY_OID;
        },
        "mz_avg_promotion_internal_v1" => Scalar {
            // Promotes a numeric type to the smallest fractional type that
            // can represent it. This is primarily useful for the avg
            // aggregate function, so that the avg of an integer column does
            // not get truncated to an integer, which would be surprising to
            // users (#549).
            params!(Float32) => Operation::identity() => Float32, oid::FUNC_MZ_AVG_PROMOTION_F32_OID_INTERNAL_V1;
            params!(Float64) => Operation::identity() => Float64, oid::FUNC_MZ_AVG_PROMOTION_F64_OID_INTERNAL_V1;
            params!(Int16) => Operation::unary(|ecx, e| {
                typeconv::plan_cast(
                    ecx, CastContext::Explicit, e, &ScalarType::Numeric {max_scale: None},
                )
            }) => Numeric, oid::FUNC_MZ_AVG_PROMOTION_I16_OID_INTERNAL_V1;
            params!(Int32) => Operation::unary(|ecx, e| {
                typeconv::plan_cast(
                    ecx, CastContext::Explicit, e, &ScalarType::Numeric {max_scale: None},
                )
            }) => Numeric, oid::FUNC_MZ_AVG_PROMOTION_I32_OID_INTERNAL_V1;
            params!(UInt16) => Operation::unary(|ecx, e| {
                typeconv::plan_cast(
                    ecx, CastContext::Explicit, e, &ScalarType::Numeric {max_scale: None},
                )
            }) => Numeric, oid::FUNC_MZ_AVG_PROMOTION_U16_OID_INTERNAL_V1;
            params!(UInt32) => Operation::unary(|ecx, e| {
                typeconv::plan_cast(
                    ecx, CastContext::Explicit, e, &ScalarType::Numeric {max_scale: None},
                )
            }) => Numeric, oid::FUNC_MZ_AVG_PROMOTION_U32_OID_INTERNAL_V1;
        },
        "mz_avg_promotion" => Scalar {
            // Promotes a numeric type to the smallest fractional type that
            // can represent it. This is primarily useful for the avg
            // aggregate function, so that the avg of an integer column does
            // not get truncated to an integer, which would be surprising to
            // users (#549).
            params!(Float32) => Operation::identity() => Float32, oid::FUNC_MZ_AVG_PROMOTION_F32_OID;
            params!(Float64) => Operation::identity() => Float64, oid::FUNC_MZ_AVG_PROMOTION_F64_OID;
            params!(Int16) => Operation::unary(|ecx, e| {
                typeconv::plan_cast(
                    ecx, CastContext::Explicit, e, &ScalarType::Numeric {max_scale: None},
                )
            }) => Numeric, oid::FUNC_MZ_AVG_PROMOTION_I16_OID;
            params!(Int32) => Operation::unary(|ecx, e| {
                typeconv::plan_cast(
                    ecx, CastContext::Explicit, e, &ScalarType::Numeric {max_scale: None},
                )
            }) => Numeric, oid::FUNC_MZ_AVG_PROMOTION_I32_OID;
            params!(Int64) => Operation::unary(|ecx, e| {
                typeconv::plan_cast(
                    ecx, CastContext::Explicit, e, &ScalarType::Numeric {max_scale: None},
                )
            }) => Numeric, oid::FUNC_MZ_AVG_PROMOTION_I64_OID;
            params!(UInt16) => Operation::unary(|ecx, e| {
                typeconv::plan_cast(
                    ecx, CastContext::Explicit, e, &ScalarType::Numeric {max_scale: None},
                )
            }) => Numeric, oid::FUNC_MZ_AVG_PROMOTION_U16_OID;
            params!(UInt32) => Operation::unary(|ecx, e| {
                typeconv::plan_cast(
                    ecx, CastContext::Explicit, e, &ScalarType::Numeric {max_scale: None},
                )
            }) => Numeric, oid::FUNC_MZ_AVG_PROMOTION_U32_OID;
            params!(UInt64) => Operation::unary(|ecx, e| {
                typeconv::plan_cast(
                    ecx, CastContext::Explicit, e, &ScalarType::Numeric {max_scale: None},
                )
            }) => Numeric, oid::FUNC_MZ_AVG_PROMOTION_U64_OID;
            params!(Numeric) => Operation::unary(|ecx, e| {
                typeconv::plan_cast(
                    ecx, CastContext::Explicit, e, &ScalarType::Numeric {max_scale: None},
                )
            }) => Numeric, oid::FUNC_MZ_AVG_PROMOTION_NUMERIC_OID;
        },
        "mz_error_if_null" => Scalar {
            // If the first argument is NULL, returns an EvalError::Internal whose error
            // message is the second argument.
            params!(Any, String) => VariadicFunc::ErrorIfNull => Any, oid::FUNC_MZ_ERROR_IF_NULL_OID;
        },
        "mz_sleep" => Scalar {
            params!(Float64) => UnaryFunc::Sleep(func::Sleep) => TimestampTz, oid::FUNC_MZ_SLEEP_OID;
        },
        "mz_panic" => Scalar {
            params!(String) => UnaryFunc::Panic(func::Panic) => String, oid::FUNC_MZ_PANIC_OID;
        }
    }
});

fn digest(algorithm: &'static str) -> Operation<HirScalarExpr> {
    Operation::unary(move |_ecx, input| {
        let algorithm = HirScalarExpr::literal(Datum::String(algorithm), ScalarType::String);
        Ok(input.call_binary(algorithm, BinaryFunc::DigestBytes))
    })
}

fn array_to_string(
    ecx: &ExprContext,
    exprs: Vec<HirScalarExpr>,
) -> Result<HirScalarExpr, PlanError> {
    let elem_type = match ecx.scalar_type(&exprs[0]) {
        ScalarType::Array(elem_type) => *elem_type,
        _ => unreachable!("array_to_string is guaranteed to receive array as first argument"),
    };
    Ok(HirScalarExpr::call_variadic(
        VariadicFunc::ArrayToString { elem_type },
        exprs,
    ))
}

/// Correlates an operator with all of its implementations.
pub static OP_IMPLS: LazyLock<BTreeMap<&'static str, Func>> = LazyLock::new(|| {
    use BinaryFunc::*;
    use ParamType::*;
    use ScalarBaseType::*;
    builtins! {
        // Literal OIDs collected from PG 13 using a version of this query
        // ```sql
        // SELECT
        //     oid,
        //     oprname,
        //     oprleft::regtype,
        //     oprright::regtype
        // FROM
        //     pg_operator
        // WHERE
        //     oprname IN (
        //         '+', '-', '*', '/', '%',
        //         '|', '&', '#', '~', '<<', '>>',
        //         '~~', '!~~'
        //     )
        // ORDER BY
        //     oprname;
        // ```
        // Values are also available through
        // https://github.com/postgres/postgres/blob/master/src/include/catalog/pg_operator.dat

        // ARITHMETIC
        "+" => Scalar {
            params!(Any) => Operation::new(|ecx, exprs, _params, _order_by| {
                // Unary plus has unusual compatibility requirements.
                //
                // In PostgreSQL, it is only defined for numeric types, so
                // `+$1` and `+'1'` get coerced to `Float64` per the usual
                // rules, but `+'1'::text` is rejected.
                //
                // In SQLite, unary plus can be applied to *any* type, and
                // is always the identity function.
                //
                // To try to be compatible with both PostgreSQL and SQlite,
                // we accept explicitly-typed arguments of any type, but try
                // to coerce unknown-type arguments as `Float64`.
                typeconv::plan_coerce(ecx, exprs.into_element(), &ScalarType::Float64)
            }) => Any, oid::OP_UNARY_PLUS_OID;
            params!(Int16, Int16) => AddInt16 => Int16, 550;
            params!(Int32, Int32) => AddInt32 => Int32, 551;
            params!(Int64, Int64) => AddInt64 => Int64, 684;
            params!(UInt16, UInt16) => AddUInt16 => UInt16, oid::FUNC_ADD_UINT16;
            params!(UInt32, UInt32) => AddUInt32 => UInt32, oid::FUNC_ADD_UINT32;
            params!(UInt64, UInt64) => AddUInt64 => UInt64, oid::FUNC_ADD_UINT64;
            params!(Float32, Float32) => AddFloat32 => Float32, 586;
            params!(Float64, Float64) => AddFloat64 => Float64, 591;
            params!(Interval, Interval) => AddInterval => Interval, 1337;
            params!(Timestamp, Interval) => AddTimestampInterval => Timestamp, 2066;
            params!(Interval, Timestamp) => {
                Operation::binary(|_ecx, lhs, rhs| Ok(rhs.call_binary(lhs, AddTimestampInterval)))
            } => Timestamp, 2553;
            params!(TimestampTz, Interval) => AddTimestampTzInterval => TimestampTz, 1327;
            params!(Interval, TimestampTz) => {
                Operation::binary(|_ecx, lhs, rhs| Ok(rhs.call_binary(lhs, AddTimestampTzInterval)))
            } => TimestampTz, 2554;
            params!(Date, Interval) => AddDateInterval => Timestamp, 1076;
            params!(Interval, Date) => {
                Operation::binary(|_ecx, lhs, rhs| Ok(rhs.call_binary(lhs, AddDateInterval)))
            } => Timestamp, 2551;
            params!(Date, Time) => AddDateTime => Timestamp, 1360;
            params!(Time, Date) => {
                Operation::binary(|_ecx, lhs, rhs| Ok(rhs.call_binary(lhs, AddDateTime)))
            } => Timestamp, 1363;
            params!(Time, Interval) => AddTimeInterval => Time, 1800;
            params!(Interval, Time) => {
                Operation::binary(|_ecx, lhs, rhs| Ok(rhs.call_binary(lhs, AddTimeInterval)))
            } => Time, 1849;
            params!(Numeric, Numeric) => AddNumeric => Numeric, 1758;
            params!(RangeAny, RangeAny) => RangeUnion => RangeAny, 3898;
        },
        "-" => Scalar {
            params!(Int16) => UnaryFunc::NegInt16(func::NegInt16) => Int16, 559;
            params!(Int32) => UnaryFunc::NegInt32(func::NegInt32) => Int32, 558;
            params!(Int64) => UnaryFunc::NegInt64(func::NegInt64) => Int64, 484;
            params!(Float32) => UnaryFunc::NegFloat32(func::NegFloat32) => Float32, 584;
            params!(Float64) => UnaryFunc::NegFloat64(func::NegFloat64) => Float64, 585;
            params!(Numeric) => UnaryFunc::NegNumeric(func::NegNumeric) => Numeric, 17510;
            params!(Interval) => UnaryFunc::NegInterval(func::NegInterval) => Interval, 1336;
            params!(Int32, Int32) => SubInt32 => Int32, 555;
            params!(Int64, Int64) => SubInt64 => Int64, 685;
            params!(UInt16, UInt16) => SubUInt16 => UInt16, oid::FUNC_SUB_UINT16;
            params!(UInt32, UInt32) => SubUInt32 => UInt32, oid::FUNC_SUB_UINT32;
            params!(UInt64, UInt64) => SubUInt64 => UInt64, oid::FUNC_SUB_UINT64;
            params!(Float32, Float32) => SubFloat32 => Float32, 587;
            params!(Float64, Float64) => SubFloat64 => Float64, 592;
            params!(Numeric, Numeric) => SubNumeric => Numeric, 17590;
            params!(Interval, Interval) => SubInterval => Interval, 1338;
            params!(Timestamp, Timestamp) => SubTimestamp => Interval, 2067;
            params!(TimestampTz, TimestampTz) => SubTimestampTz => Interval, 1328;
            params!(Timestamp, Interval) => SubTimestampInterval => Timestamp, 2068;
            params!(TimestampTz, Interval) => SubTimestampTzInterval => TimestampTz, 1329;
            params!(Date, Date) => SubDate => Int32, 1099;
            params!(Date, Interval) => SubDateInterval => Timestamp, 1077;
            params!(Time, Time) => SubTime => Interval, 1399;
            params!(Time, Interval) => SubTimeInterval => Time, 1801;
            params!(Jsonb, Int64) => JsonbDeleteInt64 => Jsonb, 3286;
            params!(Jsonb, String) => JsonbDeleteString => Jsonb, 3285;
            params!(RangeAny, RangeAny) => RangeDifference => RangeAny, 3899;
            // TODO(jamii) there should be corresponding overloads for
            // Array(Int64) and Array(String)
        },
        "*" => Scalar {
            params!(Int16, Int16) => MulInt16 => Int16, 526;
            params!(Int32, Int32) => MulInt32 => Int32, 514;
            params!(Int64, Int64) => MulInt64 => Int64, 686;
            params!(UInt16, UInt16) => MulUInt16 => UInt16, oid::FUNC_MUL_UINT16;
            params!(UInt32, UInt32) => MulUInt32 => UInt32, oid::FUNC_MUL_UINT32;
            params!(UInt64, UInt64) => MulUInt64 => UInt64, oid::FUNC_MUL_UINT64;
            params!(Float32, Float32) => MulFloat32 => Float32, 589;
            params!(Float64, Float64) => MulFloat64 => Float64, 594;
            params!(Interval, Float64) => MulInterval => Interval, 1583;
            params!(Float64, Interval) => {
                Operation::binary(|_ecx, lhs, rhs| Ok(rhs.call_binary(lhs, MulInterval)))
            } => Interval, 1584;
            params!(Numeric, Numeric) => MulNumeric => Numeric, 1760;
            params!(RangeAny, RangeAny) => RangeIntersection => RangeAny, 3900;
        },
        "/" => Scalar {
            params!(Int16, Int16) => DivInt16 => Int16, 527;
            params!(Int32, Int32) => DivInt32 => Int32, 528;
            params!(Int64, Int64) => DivInt64 => Int64, 687;
            params!(UInt16, UInt16) => DivUInt16 => UInt16, oid::FUNC_DIV_UINT16;
            params!(UInt32, UInt32) => DivUInt32 => UInt32, oid::FUNC_DIV_UINT32;
            params!(UInt64, UInt64) => DivUInt64 => UInt64, oid::FUNC_DIV_UINT64;
            params!(Float32, Float32) => DivFloat32 => Float32, 588;
            params!(Float64, Float64) => DivFloat64 => Float64, 593;
            params!(Interval, Float64) => DivInterval => Interval, 1585;
            params!(Numeric, Numeric) => DivNumeric => Numeric, 1761;
        },
        "%" => Scalar {
            params!(Int16, Int16) => ModInt16 => Int16, 529;
            params!(Int32, Int32) => ModInt32 => Int32, 530;
            params!(Int64, Int64) => ModInt64 => Int64, 439;
            params!(UInt16, UInt16) => ModUInt16 => UInt16, oid::FUNC_MOD_UINT16;
            params!(UInt32, UInt32) => ModUInt32 => UInt32, oid::FUNC_MOD_UINT32;
            params!(UInt64, UInt64) => ModUInt64 => UInt64, oid::FUNC_MOD_UINT64;
            params!(Float32, Float32) => ModFloat32 => Float32, oid::OP_MOD_F32_OID;
            params!(Float64, Float64) => ModFloat64 => Float64, oid::OP_MOD_F64_OID;
            params!(Numeric, Numeric) => ModNumeric => Numeric, 1762;
        },
        "&" => Scalar {
            params!(Int16, Int16) => BitAndInt16 => Int16, 1874;
            params!(Int32, Int32) => BitAndInt32 => Int32, 1880;
            params!(Int64, Int64) => BitAndInt64 => Int64, 1886;
            params!(UInt16, UInt16) => BitAndUInt16 => UInt16, oid::FUNC_AND_UINT16;
            params!(UInt32, UInt32) => BitAndUInt32 => UInt32, oid::FUNC_AND_UINT32;
            params!(UInt64, UInt64) => BitAndUInt64 => UInt64, oid::FUNC_AND_UINT64;
        },
        "|" => Scalar {
            params!(Int16, Int16) => BitOrInt16 => Int16, 1875;
            params!(Int32, Int32) => BitOrInt32 => Int32, 1881;
            params!(Int64, Int64) => BitOrInt64 => Int64, 1887;
            params!(UInt16, UInt16) => BitOrUInt16 => UInt16, oid::FUNC_OR_UINT16;
            params!(UInt32, UInt32) => BitOrUInt32 => UInt32, oid::FUNC_OR_UINT32;
            params!(UInt64, UInt64) => BitOrUInt64 => UInt64, oid::FUNC_OR_UINT64;
        },
        "#" => Scalar {
            params!(Int16, Int16) => BitXorInt16 => Int16, 1876;
            params!(Int32, Int32) => BitXorInt32 => Int32, 1882;
            params!(Int64, Int64) => BitXorInt64 => Int64, 1888;
            params!(UInt16, UInt16) => BitXorUInt16 => UInt16, oid::FUNC_XOR_UINT16;
            params!(UInt32, UInt32) => BitXorUInt32 => UInt32, oid::FUNC_XOR_UINT32;
            params!(UInt64, UInt64) => BitXorUInt64 => UInt64, oid::FUNC_XOR_UINT64;
        },
        "<<" => Scalar {
            params!(Int16, Int32) => BitShiftLeftInt16 => Int16, 1878;
            params!(Int32, Int32) => BitShiftLeftInt32 => Int32, 1884;
            params!(Int64, Int32) => BitShiftLeftInt64 => Int64, 1890;
            params!(UInt16, UInt32) => BitShiftLeftUInt16 => UInt16, oid::FUNC_SHIFT_LEFT_UINT16;
            params!(UInt32, UInt32) => BitShiftLeftUInt32 => UInt32, oid::FUNC_SHIFT_LEFT_UINT32;
            params!(UInt64, UInt32) => BitShiftLeftUInt64 => UInt64, oid::FUNC_SHIFT_LEFT_UINT64;
            params!(RangeAny, RangeAny) => RangeBefore => Bool, 3893;
        },
        ">>" => Scalar {
            params!(Int16, Int32) => BitShiftRightInt16 => Int16, 1879;
            params!(Int32, Int32) => BitShiftRightInt32 => Int32, 1885;
            params!(Int64, Int32) => BitShiftRightInt64 => Int64, 1891;
            params!(UInt16, UInt32) => BitShiftRightUInt16 => UInt16, oid::FUNC_SHIFT_RIGHT_UINT16;
            params!(UInt32, UInt32) => BitShiftRightUInt32 => UInt32, oid::FUNC_SHIFT_RIGHT_UINT32;
            params!(UInt64, UInt32) => BitShiftRightUInt64 => UInt64, oid::FUNC_SHIFT_RIGHT_UINT64;
            params!(RangeAny, RangeAny) => RangeAfter => Bool, 3894;
        },

        // ILIKE
        "~~*" => Scalar {
            params!(String, String) => IsLikeMatch { case_insensitive: true } => Bool, 1627;
            params!(Char, String) => Operation::binary(|ecx, lhs, rhs| {
                let length = ecx.scalar_type(&lhs).unwrap_char_length();
                Ok(lhs.call_unary(UnaryFunc::PadChar(func::PadChar { length }))
                    .call_binary(rhs, IsLikeMatch { case_insensitive: true })
                )
            }) => Bool, 1629;
        },
        "!~~*" => Scalar {
            params!(String, String) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs
                    .call_binary(rhs, IsLikeMatch { case_insensitive: true })
                    .call_unary(UnaryFunc::Not(func::Not)))
            }) => Bool, 1628;
            params!(Char, String) => Operation::binary(|ecx, lhs, rhs| {
                let length = ecx.scalar_type(&lhs).unwrap_char_length();
                Ok(lhs.call_unary(UnaryFunc::PadChar(func::PadChar { length }))
                    .call_binary(rhs, IsLikeMatch { case_insensitive: true })
                    .call_unary(UnaryFunc::Not(func::Not))
                )
            }) => Bool, 1630;
        },


        // LIKE
        "~~" => Scalar {
            params!(String, String) => IsLikeMatch { case_insensitive: false } => Bool, 1209;
            params!(Char, String) => Operation::binary(|ecx, lhs, rhs| {
                let length = ecx.scalar_type(&lhs).unwrap_char_length();
                Ok(lhs.call_unary(UnaryFunc::PadChar(func::PadChar { length }))
                    .call_binary(rhs, IsLikeMatch { case_insensitive: false })
                )
            }) => Bool, 1211;
        },
        "!~~" => Scalar {
            params!(String, String) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs
                    .call_binary(rhs, IsLikeMatch { case_insensitive: false })
                    .call_unary(UnaryFunc::Not(func::Not)))
            }) => Bool, 1210;
            params!(Char, String) => Operation::binary(|ecx, lhs, rhs| {
                let length = ecx.scalar_type(&lhs).unwrap_char_length();
                Ok(lhs.call_unary(UnaryFunc::PadChar(func::PadChar { length }))
                    .call_binary(rhs, IsLikeMatch { case_insensitive: false })
                    .call_unary(UnaryFunc::Not(func::Not))
                )
            }) => Bool, 1212;
        },

        // REGEX
        "~" => Scalar {
            params!(Int16) => UnaryFunc::BitNotInt16(func::BitNotInt16) => Int16, 1877;
            params!(Int32) => UnaryFunc::BitNotInt32(func::BitNotInt32) => Int32, 1883;
            params!(Int64) => UnaryFunc::BitNotInt64(func::BitNotInt64) => Int64, 1889;
            params!(UInt16) => UnaryFunc::BitNotUint16(func::BitNotUint16) => UInt16, oid::FUNC_BIT_NOT_UINT16_OID;
            params!(UInt32) => UnaryFunc::BitNotUint32(func::BitNotUint32) => UInt32, oid::FUNC_BIT_NOT_UINT32_OID;
            params!(UInt64) => UnaryFunc::BitNotUint64(func::BitNotUint64) => UInt64, oid::FUNC_BIT_NOT_UINT64_OID;
            params!(String, String) => IsRegexpMatch { case_insensitive: false } => Bool, 641;
            params!(Char, String) => Operation::binary(|ecx, lhs, rhs| {
                let length = ecx.scalar_type(&lhs).unwrap_char_length();
                Ok(lhs.call_unary(UnaryFunc::PadChar(func::PadChar { length }))
                    .call_binary(rhs, IsRegexpMatch { case_insensitive: false })
                )
            }) => Bool, 1055;
        },
        "~*" => Scalar {
            params!(String, String) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs.call_binary(rhs, IsRegexpMatch { case_insensitive: true }))
            }) => Bool, 1228;
            params!(Char, String) => Operation::binary(|ecx, lhs, rhs| {
                let length = ecx.scalar_type(&lhs).unwrap_char_length();
                Ok(lhs.call_unary(UnaryFunc::PadChar(func::PadChar { length }))
                    .call_binary(rhs, IsRegexpMatch { case_insensitive: true })
                )
            }) => Bool, 1234;
        },
        "!~" => Scalar {
            params!(String, String) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs
                    .call_binary(rhs, IsRegexpMatch { case_insensitive: false })
                    .call_unary(UnaryFunc::Not(func::Not)))
            }) => Bool, 642;
            params!(Char, String) => Operation::binary(|ecx, lhs, rhs| {
                let length = ecx.scalar_type(&lhs).unwrap_char_length();
                Ok(lhs.call_unary(UnaryFunc::PadChar(func::PadChar { length }))
                    .call_binary(rhs, IsRegexpMatch { case_insensitive: false })
                    .call_unary(UnaryFunc::Not(func::Not))
                )
            }) => Bool, 1056;
        },
        "!~*" => Scalar {
            params!(String, String) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs
                    .call_binary(rhs, IsRegexpMatch { case_insensitive: true })
                    .call_unary(UnaryFunc::Not(func::Not)))
            }) => Bool, 1229;
            params!(Char, String) => Operation::binary(|ecx, lhs, rhs| {
                let length = ecx.scalar_type(&lhs).unwrap_char_length();
                Ok(lhs.call_unary(UnaryFunc::PadChar(func::PadChar { length }))
                    .call_binary(rhs, IsRegexpMatch { case_insensitive: true })
                    .call_unary(UnaryFunc::Not(func::Not))
                )
            }) => Bool, 1235;
        },

        // CONCAT
        "||" => Scalar {
            params!(String, NonVecAny) => Operation::binary(|ecx, lhs, rhs| {
                let rhs = typeconv::plan_cast(
                    ecx,
                    CastContext::Explicit,
                    rhs,
                    &ScalarType::String,
                )?;
                Ok(lhs.call_binary(rhs, TextConcat))
            }) => String, 2779;
            params!(NonVecAny, String) => Operation::binary(|ecx, lhs, rhs| {
                let lhs = typeconv::plan_cast(
                    ecx,
                    CastContext::Explicit,
                    lhs,
                    &ScalarType::String,
                )?;
                Ok(lhs.call_binary(rhs, TextConcat))
            }) => String, 2780;
            params!(String, String) => TextConcat => String, 654;
            params!(Jsonb, Jsonb) => JsonbConcat => Jsonb, 3284;
            params!(ArrayAnyCompatible, ArrayAnyCompatible) => ArrayArrayConcat => ArrayAnyCompatible, 375;
            params!(ListAnyCompatible, ListAnyCompatible) => ListListConcat => ListAnyCompatible, oid::OP_CONCAT_LIST_LIST_OID;
            params!(ListAnyCompatible, ListElementAnyCompatible) => ListElementConcat => ListAnyCompatible, oid::OP_CONCAT_LIST_ELEMENT_OID;
            params!(ListElementAnyCompatible, ListAnyCompatible) => ElementListConcat => ListAnyCompatible, oid::OP_CONCAT_ELEMENY_LIST_OID;
        },

        // JSON, MAP, RANGE, LIST, ARRAY
        "->" => Scalar {
            params!(Jsonb, Int64) => JsonbGetInt64 => Jsonb, 3212;
            params!(Jsonb, String) => JsonbGetString => Jsonb, 3211;
            params!(MapAny, String) => MapGetValue => Any, oid::OP_GET_VALUE_MAP_OID;
        },
        "->>" => Scalar {
            params!(Jsonb, Int64) => JsonbGetInt64Stringify => String, 3481;
            params!(Jsonb, String) => JsonbGetStringStringify => String, 3477;
        },
        "#>" => Scalar {
            params!(Jsonb, ScalarType::Array(Box::new(ScalarType::String))) => JsonbGetPath => Jsonb, 3213;
        },
        "#>>" => Scalar {
            params!(Jsonb, ScalarType::Array(Box::new(ScalarType::String))) => JsonbGetPathStringify => String, 3206;
        },
        "@>" => Scalar {
            params!(Jsonb, Jsonb) => JsonbContainsJsonb => Bool, 3246;
            params!(Jsonb, String) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs.call_binary(
                    rhs.call_unary(UnaryFunc::CastStringToJsonb(func::CastStringToJsonb)),
                    JsonbContainsJsonb,
                ))
            }) => Bool, oid::OP_CONTAINS_JSONB_STRING_OID;
            params!(String, Jsonb) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs.call_unary(UnaryFunc::CastStringToJsonb(func::CastStringToJsonb))
                      .call_binary(rhs, JsonbContainsJsonb))
            }) => Bool, oid::OP_CONTAINS_STRING_JSONB_OID;
            params!(MapAnyCompatible, MapAnyCompatible) => MapContainsMap => Bool, oid::OP_CONTAINS_MAP_MAP_OID;
            params!(RangeAny, AnyElement) => Operation::binary(|ecx, lhs, rhs| {
                let elem_type = ecx.scalar_type(&lhs).unwrap_range_element_type().clone();
                Ok(lhs.call_binary(rhs, BinaryFunc::RangeContainsElem { elem_type, rev: false }))
            }) => Bool, 3889;
            params!(RangeAny, RangeAny) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs.call_binary(rhs, BinaryFunc::RangeContainsRange { rev: false }))
            }) => Bool, 3890;
            params!(ArrayAny, ArrayAny) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs.call_binary(rhs, BinaryFunc::ArrayContainsArray { rev: false }))
            }) => Bool, 2751;
            params!(ListAny, ListAny) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs.call_binary(rhs, BinaryFunc::ListContainsList { rev: false }))
            }) => Bool, oid::OP_CONTAINS_LIST_LIST_OID;
        },
        "<@" => Scalar {
            params!(Jsonb, Jsonb) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(rhs.call_binary(
                    lhs,
                    JsonbContainsJsonb
                ))
            }) => Bool, 3250;
            params!(Jsonb, String) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(rhs.call_unary(UnaryFunc::CastStringToJsonb(func::CastStringToJsonb))
                      .call_binary(lhs, BinaryFunc::JsonbContainsJsonb))
            }) => Bool, oid::OP_CONTAINED_JSONB_STRING_OID;
            params!(String, Jsonb) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(rhs.call_binary(
                    lhs.call_unary(UnaryFunc::CastStringToJsonb(func::CastStringToJsonb)),
                    BinaryFunc::JsonbContainsJsonb,
                ))
            }) => Bool, oid::OP_CONTAINED_STRING_JSONB_OID;
            params!(MapAnyCompatible, MapAnyCompatible) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(rhs.call_binary(lhs, MapContainsMap))
            }) => Bool, oid::OP_CONTAINED_MAP_MAP_OID;
            params!(AnyElement, RangeAny) => Operation::binary(|ecx, lhs, rhs| {
                let elem_type = ecx.scalar_type(&rhs).unwrap_range_element_type().clone();
                Ok(rhs.call_binary(lhs, BinaryFunc::RangeContainsElem { elem_type, rev: true }))
            }) => Bool, 3891;
            params!(RangeAny, RangeAny) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(rhs.call_binary(lhs, BinaryFunc::RangeContainsRange { rev: true }))
            }) => Bool, 3892;
            params!(ArrayAny, ArrayAny) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs.call_binary(rhs, BinaryFunc::ArrayContainsArray { rev: true }))
            }) => Bool, 2752;
            params!(ListAny, ListAny) => Operation::binary(|_ecx, lhs, rhs| {
                Ok(lhs.call_binary(rhs, BinaryFunc::ListContainsList { rev: true }))
            }) => Bool, oid::OP_IS_CONTAINED_LIST_LIST_OID;
        },
        "?" => Scalar {
            params!(Jsonb, String) => JsonbContainsString => Bool, 3247;
            params!(MapAny, String) => MapContainsKey => Bool, oid::OP_CONTAINS_KEY_MAP_OID;
        },
        "?&" => Scalar {
            params!(MapAny, ScalarType::Array(Box::new(ScalarType::String))) => MapContainsAllKeys => Bool, oid::OP_CONTAINS_ALL_KEYS_MAP_OID;
        },
        "?|" => Scalar {
            params!(MapAny, ScalarType::Array(Box::new(ScalarType::String))) => MapContainsAnyKeys => Bool, oid::OP_CONTAINS_ANY_KEYS_MAP_OID;
        },
        "&&" => Scalar {
            params!(RangeAny, RangeAny) => BinaryFunc::RangeOverlaps => Bool, 3888;
        },
        "&<" => Scalar {
            params!(RangeAny, RangeAny) => BinaryFunc::RangeOverleft => Bool, 3895;
        },
        "&>" => Scalar {
            params!(RangeAny, RangeAny) => BinaryFunc::RangeOverright => Bool, 3896;
        },
        "-|-" => Scalar {
            params!(RangeAny, RangeAny) => BinaryFunc::RangeAdjacent => Bool, 3897;
        },

        // COMPARISON OPS
        "<" => Scalar {
            params!(Numeric, Numeric) => BinaryFunc::Lt => Bool, 1754;
            params!(Bool, Bool) => BinaryFunc::Lt => Bool, 58;
            params!(Int16, Int16) => BinaryFunc::Lt => Bool, 95;
            params!(Int32, Int32) => BinaryFunc::Lt => Bool, 97;
            params!(Int64, Int64) => BinaryFunc::Lt => Bool, 412;
            params!(UInt16, UInt16) => BinaryFunc::Lt => Bool, oid::FUNC_LT_UINT16_OID;
            params!(UInt32, UInt32) => BinaryFunc::Lt => Bool, oid::FUNC_LT_UINT32_OID;
            params!(UInt64, UInt64) => BinaryFunc::Lt => Bool, oid::FUNC_LT_UINT64_OID;
            params!(Float32, Float32) => BinaryFunc::Lt => Bool, 622;
            params!(Float64, Float64) => BinaryFunc::Lt => Bool, 672;
            params!(Oid, Oid) => BinaryFunc::Lt => Bool, 609;
            params!(Date, Date) => BinaryFunc::Lt => Bool, 1095;
            params!(Time, Time) => BinaryFunc::Lt => Bool, 1110;
            params!(Timestamp, Timestamp) => BinaryFunc::Lt => Bool, 2062;
            params!(TimestampTz, TimestampTz) => BinaryFunc::Lt => Bool, 1322;
            params!(Uuid, Uuid) => BinaryFunc::Lt => Bool, 2974;
            params!(Interval, Interval) => BinaryFunc::Lt => Bool, 1332;
            params!(Bytes, Bytes) => BinaryFunc::Lt => Bool, 1957;
            params!(String, String) => BinaryFunc::Lt => Bool, 664;
            params!(Char, Char) => BinaryFunc::Lt => Bool, 1058;
            params!(PgLegacyChar, PgLegacyChar) => BinaryFunc::Lt => Bool, 631;
            params!(PgLegacyName, PgLegacyName) => BinaryFunc::Lt => Bool, 660;
            params!(Jsonb, Jsonb) => BinaryFunc::Lt => Bool, 3242;
            params!(ArrayAny, ArrayAny) => BinaryFunc::Lt => Bool, 1072;
            params!(RecordAny, RecordAny) => BinaryFunc::Lt => Bool, 2990;
            params!(MzTimestamp, MzTimestamp)=>BinaryFunc::Lt =>Bool, oid::FUNC_MZ_TIMESTAMP_LT_MZ_TIMESTAMP_OID;
            params!(RangeAny, RangeAny) => BinaryFunc::Lt => Bool, 3884;
        },
        "<=" => Scalar {
            params!(Numeric, Numeric) => BinaryFunc::Lte => Bool, 1755;
            params!(Bool, Bool) => BinaryFunc::Lte => Bool, 1694;
            params!(Int16, Int16) => BinaryFunc::Lte => Bool, 522;
            params!(Int32, Int32) => BinaryFunc::Lte => Bool, 523;
            params!(Int64, Int64) => BinaryFunc::Lte => Bool, 414;
            params!(UInt16, UInt16) => BinaryFunc::Lte => Bool, oid::FUNC_LTE_UINT16_OID;
            params!(UInt32, UInt32) => BinaryFunc::Lte => Bool, oid::FUNC_LTE_UINT32_OID;
            params!(UInt64, UInt64) => BinaryFunc::Lte => Bool, oid::FUNC_LTE_UINT64_OID;
            params!(Float32, Float32) => BinaryFunc::Lte => Bool, 624;
            params!(Float64, Float64) => BinaryFunc::Lte => Bool, 673;
            params!(Oid, Oid) => BinaryFunc::Lte => Bool, 611;
            params!(Date, Date) => BinaryFunc::Lte => Bool, 1096;
            params!(Time, Time) => BinaryFunc::Lte => Bool, 1111;
            params!(Timestamp, Timestamp) => BinaryFunc::Lte => Bool, 2063;
            params!(TimestampTz, TimestampTz) => BinaryFunc::Lte => Bool, 1323;
            params!(Uuid, Uuid) => BinaryFunc::Lte => Bool, 2976;
            params!(Interval, Interval) => BinaryFunc::Lte => Bool, 1333;
            params!(Bytes, Bytes) => BinaryFunc::Lte => Bool, 1958;
            params!(String, String) => BinaryFunc::Lte => Bool, 665;
            params!(Char, Char) => BinaryFunc::Lte => Bool, 1059;
            params!(PgLegacyChar, PgLegacyChar) => BinaryFunc::Lte => Bool, 632;
            params!(PgLegacyName, PgLegacyName) => BinaryFunc::Lte => Bool, 661;
            params!(Jsonb, Jsonb) => BinaryFunc::Lte => Bool, 3244;
            params!(ArrayAny, ArrayAny) => BinaryFunc::Lte => Bool, 1074;
            params!(RecordAny, RecordAny) => BinaryFunc::Lte => Bool, 2992;
            params!(MzTimestamp, MzTimestamp)=>BinaryFunc::Lte =>Bool, oid::FUNC_MZ_TIMESTAMP_LTE_MZ_TIMESTAMP_OID;
            params!(RangeAny, RangeAny) => BinaryFunc::Lte => Bool, 3885;
        },
        ">" => Scalar {
            params!(Numeric, Numeric) => BinaryFunc::Gt => Bool, 1756;
            params!(Bool, Bool) => BinaryFunc::Gt => Bool, 59;
            params!(Int16, Int16) => BinaryFunc::Gt => Bool, 520;
            params!(Int32, Int32) => BinaryFunc::Gt => Bool, 521;
            params!(Int64, Int64) => BinaryFunc::Gt => Bool, 413;
            params!(UInt16, UInt16) => BinaryFunc::Gt => Bool, oid::FUNC_GT_UINT16_OID;
            params!(UInt32, UInt32) => BinaryFunc::Gt => Bool, oid::FUNC_GT_UINT32_OID;
            params!(UInt64, UInt64) => BinaryFunc::Gt => Bool, oid::FUNC_GT_UINT64_OID;
            params!(Float32, Float32) => BinaryFunc::Gt => Bool, 623;
            params!(Float64, Float64) => BinaryFunc::Gt => Bool, 674;
            params!(Oid, Oid) => BinaryFunc::Gt => Bool, 610;
            params!(Date, Date) => BinaryFunc::Gt => Bool, 1097;
            params!(Time, Time) => BinaryFunc::Gt => Bool, 1112;
            params!(Timestamp, Timestamp) => BinaryFunc::Gt => Bool, 2064;
            params!(TimestampTz, TimestampTz) => BinaryFunc::Gt => Bool, 1324;
            params!(Uuid, Uuid) => BinaryFunc::Gt => Bool, 2975;
            params!(Interval, Interval) => BinaryFunc::Gt => Bool, 1334;
            params!(Bytes, Bytes) => BinaryFunc::Gt => Bool, 1959;
            params!(String, String) => BinaryFunc::Gt => Bool, 666;
            params!(Char, Char) => BinaryFunc::Gt => Bool, 1060;
            params!(PgLegacyChar, PgLegacyChar) => BinaryFunc::Gt => Bool, 633;
            params!(PgLegacyName, PgLegacyName) => BinaryFunc::Gt => Bool, 662;
            params!(Jsonb, Jsonb) => BinaryFunc::Gt => Bool, 3243;
            params!(ArrayAny, ArrayAny) => BinaryFunc::Gt => Bool, 1073;
            params!(RecordAny, RecordAny) => BinaryFunc::Gt => Bool, 2991;
            params!(MzTimestamp, MzTimestamp)=>BinaryFunc::Gt =>Bool, oid::FUNC_MZ_TIMESTAMP_GT_MZ_TIMESTAMP_OID;
            params!(RangeAny, RangeAny) => BinaryFunc::Gt => Bool, 3887;
        },
        ">=" => Scalar {
            params!(Numeric, Numeric) => BinaryFunc::Gte => Bool, 1757;
            params!(Bool, Bool) => BinaryFunc::Gte => Bool, 1695;
            params!(Int16, Int16) => BinaryFunc::Gte => Bool, 524;
            params!(Int32, Int32) => BinaryFunc::Gte => Bool, 525;
            params!(Int64, Int64) => BinaryFunc::Gte => Bool, 415;
            params!(UInt16, UInt16) => BinaryFunc::Gte => Bool, oid::FUNC_GTE_UINT16_OID;
            params!(UInt32, UInt32) => BinaryFunc::Gte => Bool, oid::FUNC_GTE_UINT32_OID;
            params!(UInt64, UInt64) => BinaryFunc::Gte => Bool, oid::FUNC_GTE_UINT64_OID;
            params!(Float32, Float32) => BinaryFunc::Gte => Bool, 625;
            params!(Float64, Float64) => BinaryFunc::Gte => Bool, 675;
            params!(Oid, Oid) => BinaryFunc::Gte => Bool, 612;
            params!(Date, Date) => BinaryFunc::Gte => Bool, 1098;
            params!(Time, Time) => BinaryFunc::Gte => Bool, 1113;
            params!(Timestamp, Timestamp) => BinaryFunc::Gte => Bool, 2065;
            params!(TimestampTz, TimestampTz) => BinaryFunc::Gte => Bool, 1325;
            params!(Uuid, Uuid) => BinaryFunc::Gte => Bool, 2977;
            params!(Interval, Interval) => BinaryFunc::Gte => Bool, 1335;
            params!(Bytes, Bytes) => BinaryFunc::Gte => Bool, 1960;
            params!(String, String) => BinaryFunc::Gte => Bool, 667;
            params!(Char, Char) => BinaryFunc::Gte => Bool, 1061;
            params!(PgLegacyChar, PgLegacyChar) => BinaryFunc::Gte => Bool, 634;
            params!(PgLegacyName, PgLegacyName) => BinaryFunc::Gte => Bool, 663;
            params!(Jsonb, Jsonb) => BinaryFunc::Gte => Bool, 3245;
            params!(ArrayAny, ArrayAny) => BinaryFunc::Gte => Bool, 1075;
            params!(RecordAny, RecordAny) => BinaryFunc::Gte => Bool, 2993;
            params!(MzTimestamp, MzTimestamp)=>BinaryFunc::Gte =>Bool, oid::FUNC_MZ_TIMESTAMP_GTE_MZ_TIMESTAMP_OID;
            params!(RangeAny, RangeAny) => BinaryFunc::Gte => Bool, 3886;
        },
        // Warning!
        // - If you are writing functions here that do not simply use
        //   `BinaryFunc::Eq`, you will break row equality (used in e.g.
        //   DISTINCT operations and JOINs). In short, this is totally verboten.
        // - The implementation of `BinaryFunc::Eq` is byte equality on two
        //   datums, and we enforce that both inputs to the function are of the
        //   same type in planning. However, it's possible that we will perform
        //   equality on types not listed here (e.g. `Varchar`) due to decisions
        //   made in the optimizer.
        // - Null inputs are handled by `BinaryFunc::eval` checking `propagates_nulls`.
        "=" => Scalar {
            params!(Numeric, Numeric) => BinaryFunc::Eq => Bool, 1752;
            params!(Bool, Bool) => BinaryFunc::Eq => Bool, 91;
            params!(Int16, Int16) => BinaryFunc::Eq => Bool, 94;
            params!(Int32, Int32) => BinaryFunc::Eq => Bool, 96;
            params!(Int64, Int64) => BinaryFunc::Eq => Bool, 410;
            params!(UInt16, UInt16) => BinaryFunc::Eq => Bool, oid::FUNC_EQ_UINT16_OID;
            params!(UInt32, UInt32) => BinaryFunc::Eq => Bool, oid::FUNC_EQ_UINT32_OID;
            params!(UInt64, UInt64) => BinaryFunc::Eq => Bool, oid::FUNC_EQ_UINT64_OID;
            params!(Float32, Float32) => BinaryFunc::Eq => Bool, 620;
            params!(Float64, Float64) => BinaryFunc::Eq => Bool, 670;
            params!(Oid, Oid) => BinaryFunc::Eq => Bool, 607;
            params!(Date, Date) => BinaryFunc::Eq => Bool, 1093;
            params!(Time, Time) => BinaryFunc::Eq => Bool, 1108;
            params!(Timestamp, Timestamp) => BinaryFunc::Eq => Bool, 2060;
            params!(TimestampTz, TimestampTz) => BinaryFunc::Eq => Bool, 1320;
            params!(Uuid, Uuid) => BinaryFunc::Eq => Bool, 2972;
            params!(Interval, Interval) => BinaryFunc::Eq => Bool, 1330;
            params!(Bytes, Bytes) => BinaryFunc::Eq => Bool, 1955;
            params!(String, String) => BinaryFunc::Eq => Bool, 98;
            params!(Char, Char) => BinaryFunc::Eq => Bool, 1054;
            params!(PgLegacyChar, PgLegacyChar) => BinaryFunc::Eq => Bool, 92;
            params!(PgLegacyName, PgLegacyName) => BinaryFunc::Eq => Bool, 93;
            params!(Jsonb, Jsonb) => BinaryFunc::Eq => Bool, 3240;
            params!(ListAny, ListAny) => BinaryFunc::Eq => Bool, oid::FUNC_LIST_EQ_OID;
            params!(ArrayAny, ArrayAny) => BinaryFunc::Eq => Bool, 1070;
            params!(RecordAny, RecordAny) => BinaryFunc::Eq => Bool, 2988;
            params!(MzTimestamp, MzTimestamp) => BinaryFunc::Eq => Bool, oid::FUNC_MZ_TIMESTAMP_EQ_MZ_TIMESTAMP_OID;
            params!(RangeAny, RangeAny) => BinaryFunc::Eq => Bool, 3882;
            params!(MzAclItem, MzAclItem) => BinaryFunc::Eq => Bool, oid::FUNC_MZ_ACL_ITEM_EQ_MZ_ACL_ITEM_OID;
            params!(AclItem, AclItem) => BinaryFunc::Eq => Bool, 974;
        },
        "<>" => Scalar {
            params!(Numeric, Numeric) => BinaryFunc::NotEq => Bool, 1753;
            params!(Bool, Bool) => BinaryFunc::NotEq => Bool, 85;
            params!(Int16, Int16) => BinaryFunc::NotEq => Bool, 519;
            params!(Int32, Int32) => BinaryFunc::NotEq => Bool, 518;
            params!(Int64, Int64) => BinaryFunc::NotEq => Bool, 411;
            params!(UInt16, UInt16) => BinaryFunc::NotEq => Bool, oid::FUNC_NOT_EQ_UINT16_OID;
            params!(UInt32, UInt32) => BinaryFunc::NotEq => Bool, oid::FUNC_NOT_EQ_UINT32_OID;
            params!(UInt64, UInt64) => BinaryFunc::NotEq => Bool, oid::FUNC_NOT_EQ_UINT64_OID;
            params!(Float32, Float32) => BinaryFunc::NotEq => Bool, 621;
            params!(Float64, Float64) => BinaryFunc::NotEq => Bool, 671;
            params!(Oid, Oid) => BinaryFunc::NotEq => Bool, 608;
            params!(Date, Date) => BinaryFunc::NotEq => Bool, 1094;
            params!(Time, Time) => BinaryFunc::NotEq => Bool, 1109;
            params!(Timestamp, Timestamp) => BinaryFunc::NotEq => Bool, 2061;
            params!(TimestampTz, TimestampTz) => BinaryFunc::NotEq => Bool, 1321;
            params!(Uuid, Uuid) => BinaryFunc::NotEq => Bool, 2973;
            params!(Interval, Interval) => BinaryFunc::NotEq => Bool, 1331;
            params!(Bytes, Bytes) => BinaryFunc::NotEq => Bool, 1956;
            params!(String, String) => BinaryFunc::NotEq => Bool, 531;
            params!(Char, Char) => BinaryFunc::NotEq => Bool, 1057;
            params!(PgLegacyChar, PgLegacyChar) => BinaryFunc::NotEq => Bool, 630;
            params!(PgLegacyName, PgLegacyName) => BinaryFunc::NotEq => Bool, 643;
            params!(Jsonb, Jsonb) => BinaryFunc::NotEq => Bool, 3241;
            params!(ArrayAny, ArrayAny) => BinaryFunc::NotEq => Bool, 1071;
            params!(RecordAny, RecordAny) => BinaryFunc::NotEq => Bool, 2989;
            params!(MzTimestamp, MzTimestamp) => BinaryFunc::NotEq => Bool, oid::FUNC_MZ_TIMESTAMP_NOT_EQ_MZ_TIMESTAMP_OID;
            params!(RangeAny, RangeAny) => BinaryFunc::NotEq => Bool, 3883;
            params!(MzAclItem, MzAclItem) => BinaryFunc::NotEq => Bool, oid::FUNC_MZ_ACL_ITEM_NOT_EQ_MZ_ACL_ITEM_OID;
        }
    }
});

/// Resolves the operator to a set of function implementations.
pub fn resolve_op(op: &str) -> Result<&'static [FuncImpl<HirScalarExpr>], PlanError> {
    match OP_IMPLS.get(op) {
        Some(Func::Scalar(impls)) => Ok(impls),
        Some(_) => unreachable!("all operators must be scalar functions"),
        // TODO: these require sql arrays
        // JsonContainsAnyFields
        // JsonContainsAllFields
        // TODO: these require json paths
        // JsonGetPath
        // JsonGetPathAsText
        // JsonDeletePath
        // JsonContainsPath
        // JsonApplyPathPredicate
        None => bail_unsupported!(format!("[{}]", op)),
    }
}

// Since ViewableVariables is unmaterializeable (which can't be eval'd) that
// depend on their arguments, implement directly with Hir.
fn current_settings(
    name: HirScalarExpr,
    missing_ok: HirScalarExpr,
) -> Result<HirScalarExpr, PlanError> {
    // MapGetValue returns Null if the key doesn't exist in the map.
    let expr = HirScalarExpr::call_binary(
        HirScalarExpr::call_unmaterializable(UnmaterializableFunc::ViewableVariables),
        HirScalarExpr::call_unary(name, UnaryFunc::Lower(func::Lower)),
        BinaryFunc::MapGetValue,
    );
    let expr = HirScalarExpr::if_then_else(
        missing_ok,
        expr.clone(),
        HirScalarExpr::call_variadic(
            VariadicFunc::ErrorIfNull,
            vec![
                expr,
                HirScalarExpr::literal(
                    Datum::String("unrecognized configuration parameter"),
                    ScalarType::String,
                ),
            ],
        ),
    );
    Ok(expr)
}
