#[derive(Debug, Clone, thiserror::Error)]
pub enum SqlFormatError {
    #[error(transparent)]
    FmtError(#[from] std::fmt::Error),

    #[error("Invalid parameter index {0}")]
    InvalidParameterIndex(usize),

    #[error("Confliction parameter type at index {0}: {1} != {2}")]
    ConflictingParameterType(usize, pg::Type, pg::Type),
}

use std::marker::PhantomData;

pub trait RowColumns: std::any::Any + From<pgt::Row> {}

impl<T> RowColumns for T where T: std::any::Any + From<pgt::Row> {}

pub struct Query<'a, E: RowColumns> {
    /// The query string
    pub q: String,

    /// The types of the parameters
    pub param_tys: Vec<pg::Type>,

    /// The parameters to the query
    pub params: Vec<&'a (dyn pg::ToSql + Sync + 'a)>,

    /// Reference to a cached static query
    pub cached: Option<&'static StaticQuery<E>>,
}

#[doc(hidden)]
pub struct StaticQuery<E: RowColumns> {
    pub q: String,
    pub params: Vec<pg::Type>,
    e: PhantomData<E>,
}

impl<E: RowColumns> From<Query<'_, E>> for StaticQuery<E> {
    #[inline(always)]
    fn from(q: Query<'_, E>) -> Self {
        StaticQuery {
            q: q.q,
            params: q.param_tys,
            e: PhantomData,
        }
    }
}

impl<E: RowColumns> Default for Query<'_, E> {
    fn default() -> Self {
        Query {
            q: String::with_capacity(128),
            params: Default::default(),
            param_tys: Default::default(),
            cached: None,
        }
    }
}

use crate::{
    func::Func,
    literal::Literal,
    name::Schema,
    table::{Column, Table, TableExt},
};
use std::{
    collections::hash_map::{Entry, HashMap},
    fmt::{self, Write},
};

use crate::literal::write_escaped_string_quoted;

#[allow(clippy::single_char_add_str)]
impl<E: RowColumns> Write for Query<'_, E> {
    #[inline(always)]
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.q.push_str(s);
        self.q.push_str(" ");
        Ok(())
    }

    #[inline(always)]
    fn write_char(&mut self, c: char) -> fmt::Result {
        self.q.push(c);
        self.q.push_str(" ");
        Ok(())
    }

    #[inline(always)]
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        self.q.write_fmt(args)?;
        self.q.push_str(" ");
        Ok(())
    }
}

#[allow(clippy::single_char_add_str)]
impl<'a, E: RowColumns> Query<'a, E> {
    #[inline(always)]
    pub fn inner(&mut self) -> &mut String {
        &mut self.q
    }

    #[doc(hidden)]
    #[inline(always)]
    pub fn __from_cached(cached: &'static StaticQuery<E>, params: Vec<&'a (dyn pg::ToSql + Sync)>) -> Self {
        Query {
            params,
            cached: Some(cached),

            // these two don't need to allocate
            q: String::new(),
            param_tys: Vec::new(),
        }
    }

    pub fn param<const DYNAMIC: bool>(
        &mut self,
        value: &'a (dyn pg::ToSql + Sync),
        ty: pg::Type,
    ) -> Result<(), SqlFormatError> {
        let idx = match self.params.iter().position(|&p| {
            // SAFETY: Worst-case parameter duplication, best-case using codegen-units=1 no issues at all
            std::ptr::eq(
                p as *const (dyn pg::ToSql + Sync),
                value as *const (dyn pg::ToSql + Sync),
            )
        }) {
            Some(idx) if DYNAMIC => {
                if ty != pg::Type::ANY {
                    let existing_ty = &self.param_tys[idx];
                    if *existing_ty == pg::Type::ANY {
                        self.param_tys[idx] = ty;
                    } else if *existing_ty != ty {
                        return Err(SqlFormatError::ConflictingParameterType(idx, ty, existing_ty.clone()));
                    }
                }

                idx + 1 // 1-indexed
            }
            v => {
                if !DYNAMIC && v.is_some() {
                    eprintln!("Duplicate {ty} parameter at index {}", v.unwrap());
                }

                self.param_tys.push(ty);
                self.params.push(value);
                self.params.len() // 1-indexed, take len after push
            }
        };

        self.inner().push_str("$");
        self.write_literal(idx as i64).map_err(From::from)
    }

    #[inline(always)]
    pub fn write_literal<L: Literal>(&mut self, lit: L) -> fmt::Result {
        lit.write_literal(self.inner(), 0)
    }

    #[inline(always)]
    pub fn write_column<T: TableExt>(&mut self, col: T, name: &'static str) -> fmt::Result {
        write!(
            self.inner(),
            "\"{}\".\"{}\"",
            if name == T::TYPENAME_SNAKE { <T as Table>::NAME.name() } else { name },
            <T as Column>::name(&col)
        )
    }

    #[inline(always)]
    pub fn write_str(&mut self, s: &str) {
        self.inner().push_str(s)
    }

    #[inline]
    pub fn write_table<T: Table>(&mut self) -> fmt::Result {
        match (T::ALIAS, T::SCHEMA) {
            (None, Schema::None) => write!(self, "\"{}\"", T::NAME.name()),
            (None, Schema::Named(name)) => write!(self, "\"{}\".\"{}\"", name, T::NAME.name()),
            (Some(alias), Schema::None) => write!(self, "\"{}\" AS {alias}", T::NAME.name()),
            (Some(alias), Schema::Named(name)) => write!(self, "\"{}\".\"{}\" AS {alias}", name, T::NAME.name()),
        }
    }

    pub fn write_column_name<C: Column>(&mut self, col: C) -> fmt::Result {
        write!(self.inner(), "\"{}\" ", col.name())
    }

    #[inline(always)]
    pub fn write_func<F: Func>(&mut self) {
        self.inner().push_str(F::NAME)
    }
}

/// Generate SQL syntax with an SQL-like macro. To make it work with a regular Rust macro
/// certain syntax had to be changed.
///
/// * For function calls `.func()` is converted to `func()`
///     * Runtime function names can be specified with `.{"whatever fmt::Display value"}()`
/// * `--` is converted to `$$`
/// * `::{let ty = Type::INT8_ARRAY; ty}` with any arbitrary code block can be used for dynamic cast types
/// * All string literals (`"string literal"`) are properly escaped and formatted as `'string literal'`
///     * Other literals such as bools and numbers are also properly formatted
/// * Known PostgreSQL Keywords are allowed through, `sql!(SELECT * FROM TestTable)`
/// * Non-keyword identifiers are treated as [`Table`](crate::Table) types.
/// * `Ident::Ident` is treated as a column, so `TestTable::Col` converts to `"test_table"."col"`
///     * `AS Ident::Ident` is treated specially to remove all but the column name for alises.
/// * Arbitrary expressions are allowed with code-blocks `{let x = 10; x + 21}`, but will be converted to [`Literal`](crate::Literal) values.
///     * To escape this behavior, prefix the code block with `@`, so `@{"something weird"}` is added directly as `something weird`, not a string.
/// * Parametric values can be specified with `#{1}` or `#{2 => Type::INT8}` for accumulating types
/// * For-loops in codegen are supported like `for your_variable in your_data { SELECT {your_variable} }
/// * Conditionals are supported via `if condition { SELECT "true" }`
///     * Also supports an `else { SELECT "false" }` branch
#[macro_export]
macro_rules! sql {
    ($($tt:tt)*) => {{
        #[allow(clippy::redundant_closure_call, unreachable_code, unused_braces, non_local_definitions)]
        (|| -> Result<_, $crate::macros::SqlFormatError> {
            use std::fmt::Write;
            use $crate::*;
            use $crate::pgt::{types::{Type, FromSql}, Error as PgError};

            #[repr(transparent)]
            pub struct Columns($crate::pgt::Row);

            impl From<$crate::pgt::Row> for Columns {
                #[inline(always)]
                fn from(row: $crate::pgt::Row) -> Self {
                    Columns(row)
                }
            }

            impl std::ops::Deref for Columns {
                type Target = $crate::pgt::Row;

                #[inline(always)]
                fn deref(&self) -> &Self::Target {
                    &self.0
                }
            }

            $crate::thorn_macros::__isql2!($crate $($tt)*);
        }())
    }};
}

#[cfg(test)]
mod tests {
    use crate::pg::Type;
    use crate::table::*;

    use crate::func::test_fn;

    crate::tables! {
        pub struct TestTable as "renamed" in MySchema {
            SomeCol: Type::INT8,
            SomeCol2: Type::INT8,
        }

        pub struct AnonTable {
            Other: Type::BOOL,
        }
    }

    #[test]
    fn test_sql_macro() {
        let y = 21;
        let k = [String::from("test"); 1];

        let res = sql! {
            use std::borrow::{Cow, Borrow};

            SELECT 1 AS @SomCol
        };

        // random hodgepodge of symbols to test the macro
        #[allow(clippy::let_and_return)]
        let res = sql! {
            WITH AnonTable (Other) AS (
                SELECT TestTable.SomeCol::{let ty = Type::BIT_ARRAY; ty} AS AnonTable.Other FROM TestTable
            )
            $$ $$
            join("%") i in [1, 2, 3] {
                SELECT {i}
            }

            {"test"}(1)

            for v in k {
                SELECT {v}
            }

            .test_fn(1, (1, 2, 3, 4))

            {"test"}(1)

            if 1 == 1 {
                SELECT {"true"}
            } else if 2 != 2 {
                SELECT "false"
            } else {
                TRUE

                // triggers compile_fail!
                //SELECT 1 AS @TestTable.SomeCol
            }

            if let Some(value) = Some("") {
                SELECT {value}
            }

            if true { 1 }

            let value = 1;

            AND  call(1, 2, 3)

            if let Some(v) = Some(1) {

            }

            match value {
                2 => {},
                1 | 3 if true => {
                    SELECT "ONE"
                },
                _ => {},
            }

            for (idx, term) in [1, 2, 3].iter().enumerate() {
                match idx {
                    2 => {},
                    1 | 3 if true => {
                        if false {
                        SELECT "TWO"
                        }
                    },
                    _ => {},
                }
            }

            SELECT AliasTable.SomeCol

            FROM TestTable AS AliasTable

            INSERT INTO TestTable (
                SomeCol
            )

            UPDATE ONLY TestTable SET (SomeCol) = (1)
            UPDATE TestTable AS Test SET (SomeCol) = (1)

            DO UPDATE TestTable SET (SomeCol) = (1)

            TestTable./SomeCol

            ARRAY_AGG()
            $$ () && || |
            SELECT SIMILAR TO TestTable.SomeCol
            FROM[#{&"test" as Type::TEXT}, 30]::int8_array #{&23 as Type::TEXT} ; call_func({y}) "hel'lo"::text[] @{"'"}     { let x = 10; x + y } !! TestTable WHERE < AND NOT = #{&1 as Type::INT8}

            1 AS @SomeCol,
            TestTable.SomeCol2 AS @_

            SELECT
        }
        .unwrap();

        println!("OUT: {}", res.q);
    }
}
