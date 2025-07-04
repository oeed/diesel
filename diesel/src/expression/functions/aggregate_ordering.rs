use self::private::SqlOrdAggregate;
use crate::expression::functions::declare_sql_function;

#[declare_sql_function]
extern "SQL" {
    /// Represents a SQL `MAX` function. This function can only take types which are
    /// ordered.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # include!("../../doctest_setup.rs");
    /// # use diesel::dsl::*;
    /// #
    /// # fn main() {
    /// #     use schema::animals::dsl::*;
    /// #     let connection = &mut establish_connection();
    /// assert_eq!(Ok(Some(8)), animals.select(max(legs)).first(connection));
    /// # }
    #[aggregate]
    fn max<ST: SqlOrdAggregate>(expr: ST) -> ST::Ret;

    /// Represents a SQL `MIN` function. This function can only take types which are
    /// ordered.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # include!("../../doctest_setup.rs");
    /// # use diesel::dsl::*;
    /// #
    /// # fn main() {
    /// #     use schema::animals::dsl::*;
    /// #     let connection = &mut establish_connection();
    /// assert_eq!(Ok(Some(4)), animals.select(min(legs)).first(connection));
    /// # }
    #[aggregate]
    fn min<ST: SqlOrdAggregate>(expr: ST) -> ST::Ret;
}
/// TODO: avoid needing this public: should be possible once windowing functions are available
pub mod private {
    use crate::sql_types::{IntoNullable, SingleValue, SqlOrd, SqlType};
    /// TODO: avoid needing this public: should be possible once windowing functions are available
    pub trait SqlOrdAggregate: SingleValue {
        type Ret: SqlType + SingleValue;
    }

    impl<T> SqlOrdAggregate for T
    where
        T: SqlOrd + IntoNullable + SingleValue,
        T::Nullable: SqlType + SingleValue,
    {
        type Ret = T::Nullable;
    }
}
