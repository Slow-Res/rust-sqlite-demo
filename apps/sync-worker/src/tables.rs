//! The tables copied to Postgres, and the plain row type that carries them.
//!
//! Adding a synced table takes three steps (spec §6.9):
//! 1. a SQLite migration with the table and its three triggers;
//! 2. a Postgres migration with the mirror table;
//! 3. one entry in [`TABLES`].
//!
//! The contract test (`tests/contract.rs`) checks that all three agree.

use anyhow::bail;

/// The two kinds of value these tables hold (SQLite INTEGER or TEXT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Text,
}

impl Kind {
    /// The Postgres type used for this kind, e.g. in `UNNEST($2::bigint[])`.
    pub fn pg_type(self) -> &'static str {
        match self {
            Kind::Int => "bigint",
            Kind::Text => "text",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub name: &'static str,
    pub kind: Kind,
    pub nullable: bool,
}

const fn col(name: &'static str, kind: Kind, nullable: bool) -> Column {
    Column {
        name,
        kind,
        nullable,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Table {
    pub name: &'static str,
    /// In the table's column order. The first column is always `id`.
    pub columns: &'static [Column],
}

impl Table {
    /// `id, title, author, ...`, for SELECT and INSERT column lists.
    pub fn column_list(&self) -> String {
        self.columns
            .iter()
            .map(|c| c.name)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Every synced table, in the order batches list them.
pub static TABLES: &[Table] = &[
    Table {
        name: "books",
        columns: &[
            col("id", Kind::Int, false),
            col("title", Kind::Text, false),
            col("author", Kind::Text, false),
            col("isbn", Kind::Text, true),
            col("published_year", Kind::Int, true),
            col("total_copies", Kind::Int, false),
            col("created_at", Kind::Int, false),
            col("updated_at", Kind::Int, false),
        ],
    },
    Table {
        name: "members",
        columns: &[
            col("id", Kind::Int, false),
            col("name", Kind::Text, false),
            col("email", Kind::Text, false),
            col("created_at", Kind::Int, false),
            col("updated_at", Kind::Int, false),
        ],
    },
    Table {
        name: "loans",
        columns: &[
            col("id", Kind::Int, false),
            col("book_id", Kind::Int, false),
            col("member_id", Kind::Int, false),
            col("borrowed_at", Kind::Int, false),
            col("due_at", Kind::Int, false),
            col("returned_at", Kind::Int, true),
        ],
    },
];

/// Find a synced table by the name stored in `change_log.table_name`.
pub fn by_name(name: &str) -> Option<&'static Table> {
    TABLES.iter().find(|t| t.name == name)
}

/// One column value. These tables hold only integers, text and NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Text(String),
    Null,
}

impl Value {
    /// The value of an integer column; `None` for NULL.
    pub fn as_int(&self) -> anyhow::Result<Option<i64>> {
        match self {
            Value::Int(v) => Ok(Some(*v)),
            Value::Null => Ok(None),
            Value::Text(_) => bail!("expected an integer, found text"),
        }
    }

    /// The value of a text column; `None` for NULL.
    pub fn as_text(&self) -> anyhow::Result<Option<String>> {
        match self {
            Value::Text(v) => Ok(Some(v.clone())),
            Value::Null => Ok(None),
            Value::Int(_) => bail!("expected text, found an integer"),
        }
    }
}

/// A row's values in its table's column order; `row[0]` is the id.
pub type Row = Vec<Value>;

/// The id of a row: its first column.
pub fn row_id(row: &Row) -> Option<i64> {
    match row.first() {
        Some(Value::Int(id)) => Some(*id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_starts_with_a_non_null_integer_id() {
        for table in TABLES {
            assert_eq!(
                table.columns[0],
                col("id", Kind::Int, false),
                "{}",
                table.name
            );
        }
    }

    #[test]
    fn table_and_column_names_are_unique() {
        let mut names: Vec<_> = TABLES.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TABLES.len());
        for table in TABLES {
            let mut columns: Vec<_> = table.columns.iter().map(|c| c.name).collect();
            columns.sort_unstable();
            columns.dedup();
            assert_eq!(columns.len(), table.columns.len(), "{}", table.name);
        }
    }

    #[test]
    fn tables_are_found_by_name() {
        assert_eq!(by_name("loans").map(|t| t.name), Some("loans"));
        assert!(
            by_name("users").is_none(),
            "users are never synced (spec A4)"
        );
        assert!(by_name("sessions").is_none());
    }

    #[test]
    fn column_list_is_comma_separated() {
        assert_eq!(
            by_name("members").unwrap().column_list(),
            "id, name, email, created_at, updated_at"
        );
    }

    #[test]
    fn values_convert_and_refuse_the_wrong_kind() {
        assert_eq!(Value::Int(3).as_int().unwrap(), Some(3));
        assert_eq!(Value::Null.as_int().unwrap(), None);
        assert!(Value::Text("x".into()).as_int().is_err());
        assert_eq!(Value::Text("x".into()).as_text().unwrap(), Some("x".into()));
        assert_eq!(Value::Null.as_text().unwrap(), None);
        assert!(Value::Int(1).as_text().is_err());
        assert_eq!(row_id(&vec![Value::Int(7), Value::Null]), Some(7));
        assert_eq!(row_id(&vec![]), None);
    }
}
