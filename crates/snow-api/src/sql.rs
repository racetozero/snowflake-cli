//! `SqlExecutionMixin`: running statements on the CLI connection.

use crate::connect::db_error_to_cli;
use crate::context::Cli;
use crate::errors::Result;
use crate::output::Output;
use crate::splitter::split_statements;
use snow_connector::{Cursor, DbError};

/// Python `textwrap.dedent`.
pub fn dedent(s: &str) -> String {
    snow_rich::render::dedent(s)
}

impl Cli {
    /// Execute one statement and return the raw error.
    pub fn try_execute(&mut self, sql: &str) -> std::result::Result<Cursor, DbError> {
        let conn = match self.connection() {
            Ok(c) => c,
            Err(e) => {
                return Err(DbError::new(
                    snow_connector::ErrorKind::Database,
                    -1,
                    None,
                    e.message(),
                ));
            }
        };
        conn.execute(sql)
    }

    /// `execute_query`: dedent, split, run each statement, return the last cursor.
    pub fn execute_query(&mut self, sql: &str) -> Result<Cursor> {
        let text = dedent(sql);
        let statements = split_statements(&text, false);
        let verbose = self.connector_info;
        let conn = self.connection()?;
        let mut last = Cursor::default();
        for st in statements {
            last = conn
                .execute(&st.text)
                .map_err(|e| db_error_to_cli(&e, verbose))?;
        }
        Ok(last)
    }

    /// `execute_query` that hands back the connector error (`except ProgrammingError`).
    pub fn try_query(
        &mut self,
        sql: &str,
    ) -> Result<std::result::Result<Cursor, snow_connector::DbError>> {
        let text = dedent(sql);
        let statements = split_statements(&text, false);
        let conn = self.connection()?;
        let mut last = Cursor::default();
        for st in statements {
            match conn.execute(&st.text) {
                Ok(c) => last = c,
                Err(e) => return Ok(Err(e)),
            }
        }
        Ok(Ok(last))
    }

    /// Execute exactly the given text as one statement.
    pub fn execute_one(&mut self, sql: &str) -> Result<Cursor> {
        let verbose = self.connector_info;
        let conn = self.connection()?;
        conn.execute(sql).map_err(|e| db_error_to_cli(&e, verbose))
    }

    /// `execute_queries`: run each statement (comments removed), return every cursor.
    pub fn execute_queries(&mut self, sql: &str) -> Result<Vec<Cursor>> {
        let text = dedent(sql);
        let statements = split_statements(&text, true);
        let verbose = self.connector_info;
        let conn = self.connection()?;
        let mut out = Vec::new();
        for st in statements {
            out.push(
                conn.execute(&st.text)
                    .map_err(|e| db_error_to_cli(&e, verbose))?,
            );
        }
        Ok(out)
    }

    pub fn timezone(&mut self) -> String {
        match self.connection_if_open() {
            Some(c) => crate::values::session_timezone(c),
            None => "UTC".into(),
        }
    }

    /// `QueryResult(cursor)` for this CLI's output format.
    pub fn query_result(&mut self, cursor: &Cursor) -> Output {
        let tz = self.timezone();
        crate::values::query_result(cursor, &tz, self.format == crate::output::Format::JsonExt)
    }

    pub fn single_query_result(&mut self, cursor: &Cursor) -> Output {
        let tz = self.timezone();
        crate::values::single_query_result(
            cursor,
            &tz,
            self.format == crate::output::Format::JsonExt,
        )
    }

    /// `FQN.using_connection(conn)`: fill database/schema from the session.
    pub fn using_connection(&mut self, f: Fqn) -> Result<Fqn> {
        let conn = self.connection()?;
        let s = conn.session.clone();
        let u = conn.user.clone();
        Ok(f.using_session(s.database.as_deref(), s.schema.as_deref(), u.as_deref()))
    }

    /// `str(cursor.fetchone()[0])`.
    pub fn first_cell(cursor: &Cursor) -> String {
        cursor
            .rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .unwrap_or_else(|| "None".into())
    }

    pub fn rows(&mut self, cursor: &Cursor) -> Vec<crate::output::Row> {
        let tz = self.timezone();
        crate::values::rows(cursor, &tz, false)
    }
}

use crate::identifiers::{Fqn, identifier_to_show_like_pattern, to_identifier, unquote_identifier};
use crate::output::{Pv, Row};

impl Cli {
    /// `check_database_and_schema_provided`.
    pub fn check_database_and_schema(&mut self, name: &str) -> Result<()> {
        let s = self.connection()?.session.clone();
        let user = self.connection()?.user.clone();
        let fqn = Fqn::from_string(name)?.using_session(
            s.database.as_deref(),
            s.schema.as_deref(),
            user.as_deref(),
        );
        if fqn.database.is_none() {
            return Err(crate::SnowError::cli(
                "Database not specified. Please update connection to add `database` parameter, or re-run command using `--database` option. Use `snow connection list` to list existing connections.",
            )
            .named("DatabaseNotProvidedError"));
        }
        if fqn.schema.is_none() {
            return Err(crate::SnowError::cli(
                "Schema not specified. Please update connection to add `schema` parameter, or re-run command using `--schema` option. Use `snow connection list` to list existing connections.",
            )
            .named("SchemaNotProvidedError"));
        }
        Ok(())
    }

    /// `_execute_schema_query`.
    pub fn execute_schema_query(&mut self, sql: &str, name: &str) -> Result<Cursor> {
        self.check_database_and_schema(name)?;
        self.execute_query(sql)
    }

    /// `show_specific_object`: the row whose `name_col` equals the unquoted name.
    pub fn show_specific_object(
        &mut self,
        plural: &str,
        name: &str,
        name_col: &str,
        in_clause: &str,
        check_schema: bool,
    ) -> Result<Option<Row>> {
        let fqn = Fqn::from_string(name)?;
        let name_in = if let Some(d) = &fqn.database {
            Some(format!(
                "in schema {d}.{}",
                fqn.schema.as_deref().unwrap_or("PUBLIC")
            ))
        } else {
            fqn.schema.as_ref().map(|s| format!("in schema {s}"))
        };
        let in_clause = match (in_clause.is_empty(), name_in) {
            (false, Some(_)) => {
                return Err(crate::SnowError::unexpected(
                    "non-empty 'in_clause' passed with qualified 'name'",
                ));
            }
            (true, Some(n)) => n,
            _ => in_clause.to_string(),
        };
        let q = format!(
            "show {plural} like {} {in_clause}",
            identifier_to_show_like_pattern(&fqn.name)
        )
        .trim()
        .to_string();
        let c = if check_schema {
            self.execute_schema_query(&q, name)?
        } else {
            self.execute_query(&q)?
        };
        if c.rows.len() > 1 {
            return Err(crate::SnowError::unexpected(format!(
                "Received multiple rows from result of SQL statement: {q}. Usage of 'show_specific_object' may not be properly scoped."
            )));
        }
        let want = unquote_identifier(&fqn.name);
        let rows = self.rows(&c);
        Ok(rows.into_iter().find(|r| {
            r.get(name_col)
                .is_some_and(|v| matches!(v, Pv::Str(s) if *s == want))
        }))
    }

    /// `current_role()`.
    pub fn current_role(&mut self) -> Result<Option<String>> {
        let c = self.execute_query("select current_role()")?;
        Ok(c.rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .filter(|s| !s.is_empty())
            .map(|r| to_identifier(&r)))
    }

    /// Enter `use_role(new_role)`; returns the role to restore afterwards.
    pub fn use_role_enter(&mut self, new_role: &str) -> Result<Option<String>> {
        let new_role = to_identifier(new_role);
        let prev = self.current_role()?;
        let different = prev
            .as_deref()
            .is_none_or(|p| p.to_lowercase() != new_role.to_lowercase());
        if different {
            self.execute_query(&format!("use role {new_role}"))?;
            return Ok(prev);
        }
        Ok(None)
    }

    pub fn use_role_exit(&mut self, prev: Option<String>) -> Result<()> {
        if let Some(p) = prev {
            self.execute_query(&format!("use role {p}"))?;
        }
        Ok(())
    }

    /// `session_has_warehouse()`.
    pub fn session_has_warehouse(&mut self) -> Result<bool> {
        let c = self.execute_query("select current_warehouse() is not null")?;
        let v = c
            .rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .unwrap_or_default();
        // `bool(row[0])`: a boolean column arrives as true/false, anything else by truthiness.
        Ok(!(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false")))
    }
}

/// `sanitize_for_terminal`: strip ANSI escape sequences.
pub fn sanitize_for_terminal(text: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\x1B(?:[@-Z\\\-_]|\[[0-?]*[ -/]*[@-~])").expect("re")
    });
    RE.replace_all(text, "").into_owned()
}
