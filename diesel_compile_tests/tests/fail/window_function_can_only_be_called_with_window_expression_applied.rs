extern crate diesel;

use diesel::*;

table! {
    users {
        id -> Integer,
        name -> Text,
    }
}

fn main() {
    use diesel::dsl::*;

    users::table.select(lag(users::name));
    //~^ ERROR: the trait bound `lag<Text, name>: ValidGrouping<()>` is not satisfied

    users::table.select(rank());
    //~^ ERROR: the trait bound `diesel::expression::functions::window_functions::rank_utils::rank: ValidGrouping<()>` is not satisfied
}
