//! Batched `INSERT` statements, runnable straight through `psql` or `mysql`.
//!
//! Output-only: seedle never reads `.sql` back, because doing so would require a
//! full dialect parser. The reader in [`crate::format`] says so explicitly
//! rather than failing obscurely.

use anyhow::Result;

use crate::config::ResolvedTable;
use crate::dialect::Dialect;
use crate::format::{Output, RowWriter};
use crate::io::LineBuffer;
use crate::schema::{Column, Table};
use crate::value::Value;

pub struct SqlWriter<'a> {
    path: String,
    columns: Vec<&'a Column>,
    dialect: &'a dyn Dialect,
    /// `INSERT INTO "t" ("a", "b") VALUES`, computed once.
    header: String,
    /// `ON CONFLICT …`, appended after the last tuple of each batch.
    conflict: String,
    batch_size: usize,
    batch: Vec<String>,
    buf: LineBuffer,
}

impl<'a> SqlWriter<'a> {
    pub fn new(
        path: String,
        table: &Table,
        columns: Vec<&'a Column>,
        cfg: &ResolvedTable,
        dialect: &'a dyn Dialect,
        batch_size: usize,
    ) -> Result<Self> {
        let key: Vec<String> = cfg
            .key
            .clone()
            .or_else(|| table.upsert_key().map(|k| k.to_vec()))
            .unwrap_or_default();
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();

        let header = format!(
            "{} {} ({}) VALUES",
            dialect.insert_verb(cfg.load_mode),
            dialect.quote_table(&table.id),
            columns
                .iter()
                .map(|c| dialect.quote_ident(&c.name))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let conflict = dialect.conflict_clause(table, &key, &names, cfg.load_mode)?;

        Ok(Self {
            path,
            columns,
            dialect,
            header,
            conflict,
            batch_size: batch_size.max(1),
            batch: Vec::new(),
            buf: LineBuffer::new(),
        })
    }

    fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        self.buf.push_line(&self.header);
        let last = self.batch.len() - 1;
        for (i, tuple) in self.batch.iter().enumerate() {
            let terminator = if i == last {
                format!("{};", self.conflict)
            } else {
                ",".into()
            };
            self.buf.push_line(&format!("  {tuple}{terminator}"));
        }
        self.batch.clear();
    }
}

impl RowWriter for SqlWriter<'_> {
    fn write_row(&mut self, row: &[Value]) -> Result<()> {
        let literals: Vec<String> = self
            .columns
            .iter()
            .zip(row)
            .map(|(col, v)| self.dialect.literal(col, v))
            .collect::<Result<Vec<_>>>()?;
        self.batch.push(format!("({})", literals.join(", ")));
        if self.batch.len() >= self.batch_size {
            self.flush();
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<Vec<Output>> {
        self.flush();
        Ok(vec![Output {
            path: self.path,
            bytes: self.buf.finish(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Format, Layout, LoadMode};
    use crate::dialect::{Mysql, Postgres};
    use crate::schema::{TableId, TypeClass};

    fn col(name: &str, class: TypeClass) -> Column {
        Column {
            name: name.into(),
            sql_type: class.label(),
            class,
            nullable: true,
            has_default: false,
            generated: false,
            identity: false,
        }
    }

    fn table(cols: &[Column]) -> Table {
        Table {
            id: TableId::new("public", "users"),
            columns: cols.to_vec(),
            primary_key: vec!["id".into()],
            unique: vec![],
            foreign_keys: vec![],
        }
    }

    fn cfg(mode: LoadMode) -> ResolvedTable {
        ResolvedTable {
            id: TableId::new("public", "users"),
            config_key: "users".into(),
            filter: None,
            order_by: vec![],
            limit: None,
            columns: None,
            exclude_columns: vec![],
            format: Format::Sql,
            layout: Layout::Single,
            pretty: false,
            json: crate::config::JsonMode::Unroll,
            load_mode: mode,
            key: None,
        }
    }

    fn render(dialect: &dyn Dialect, mode: LoadMode, batch: usize, rows: &[Vec<Value>]) -> String {
        let cols = vec![
            col("id", TypeClass::Int { bits: 32 }),
            col("name", TypeClass::Text { max_len: None }),
        ];
        let t = table(&cols);
        let refs: Vec<&Column> = cols.iter().collect();
        let mut w = Box::new(
            SqlWriter::new("users.sql".into(), &t, refs, &cfg(mode), dialect, batch).unwrap(),
        );
        for r in rows {
            w.write_row(r).unwrap();
        }
        String::from_utf8(w.finish().unwrap()[0].bytes.clone()).unwrap()
    }

    fn two_rows() -> Vec<Vec<Value>> {
        vec![
            vec![Value::Int(1), Value::Text("a".into())],
            vec![Value::Int(2), Value::Null],
        ]
    }

    #[test]
    fn emits_one_statement_per_batch() {
        let out = render(&Postgres, LoadMode::Insert, 100, &two_rows());
        assert_eq!(
            out,
            "INSERT INTO \"public\".\"users\" (\"id\", \"name\") VALUES\n  \
             (1, 'a'::text),\n  (2, NULL);\n"
        );
    }

    #[test]
    fn batching_splits_into_separate_statements() {
        let out = render(&Postgres, LoadMode::Insert, 1, &two_rows());
        assert_eq!(out.matches("INSERT INTO").count(), 2);
        assert_eq!(out.matches(';').count(), 2);
    }

    #[test]
    fn the_conflict_clause_lands_once_per_statement_at_the_end() {
        let out = render(&Postgres, LoadMode::Upsert, 100, &two_rows());
        assert_eq!(out.matches("ON CONFLICT").count(), 1);
        assert!(
            out.contains(
                "(2, NULL) ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\";"
            ),
            "{out}"
        );
        // And it must not appear after a non-final tuple.
        assert!(!out.contains("'a'::text) ON CONFLICT"), "{out}");
    }

    #[test]
    fn each_batch_gets_its_own_conflict_clause() {
        let out = render(&Postgres, LoadMode::Upsert, 1, &two_rows());
        assert_eq!(out.matches("ON CONFLICT").count(), 2);
    }

    #[test]
    fn mysql_uses_its_own_verb_and_clause() {
        let out = render(&Mysql, LoadMode::Upsert, 100, &two_rows());
        assert!(
            out.starts_with("INSERT INTO `public`.`users` (`id`, `name`) VALUES"),
            "{out}"
        );
        assert!(
            out.contains("ON DUPLICATE KEY UPDATE `name` = VALUES(`name`)"),
            "{out}"
        );

        let out = render(&Mysql, LoadMode::SkipExisting, 100, &two_rows());
        assert!(out.starts_with("INSERT IGNORE INTO"), "{out}");
        assert!(!out.contains("ON DUPLICATE"), "{out}");
    }

    #[test]
    fn hostile_text_is_escaped_inside_the_literal() {
        let rows = vec![vec![
            Value::Int(1),
            Value::Text("'); DROP TABLE users; --".into()),
        ]];
        let out = render(&Postgres, LoadMode::Insert, 10, &rows);
        assert!(out.contains("'''); DROP TABLE users; --'::text"), "{out}");
        // The two semicolons inside the payload plus the statement's own, and a
        // single INSERT: nothing escaped the literal to become new SQL.
        assert_eq!(out.matches(';').count(), 3, "{out}");
        assert_eq!(out.matches("INSERT INTO").count(), 1);
        assert_eq!(
            out.matches("DROP TABLE").count(),
            1,
            "still just data: {out}"
        );
    }

    #[test]
    fn no_rows_produces_an_empty_file_not_a_dangling_insert() {
        let out = render(&Postgres, LoadMode::Insert, 10, &[]);
        assert!(out.is_empty(), "expected nothing, got {out:?}");
    }

    #[test]
    fn upsert_without_a_key_fails_at_construction_not_mid_file() {
        let cols = vec![col("a", TypeClass::Int { bits: 32 })];
        let mut t = table(&cols);
        t.primary_key.clear();
        let refs: Vec<&Column> = cols.iter().collect();
        let err = match SqlWriter::new(
            "t.sql".into(),
            &t,
            refs,
            &cfg(LoadMode::Upsert),
            &Postgres,
            10,
        ) {
            Ok(_) => panic!("an upsert with no key must not construct"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("no primary key"), "{err}");
    }
}
