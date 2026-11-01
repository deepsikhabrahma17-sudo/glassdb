//! Comparing what the engine says about the past with what the model saved.

use glassdb_core::time::format_timestamp;
use glassdb_core::{Error, Output, Value};

use super::{NAMES, Row, Run, State, model_rows, pairs, rows_of};

impl Run {
    pub fn check_readers(&self) {
        for (reader, state) in &self.readers {
            for table in NAMES {
                let found = reader.query(&format!("SELECT id, n FROM {table} ORDER BY id"));
                match (found, model_rows(state, table)) {
                    (Ok(found), Some(expected)) => {
                        assert_eq!(
                            pairs(&rows_of(found)),
                            expected,
                            "reader at {}",
                            reader.txn()
                        )
                    }
                    (Err(Error::Query(msg)), None) => assert!(msg.contains("no table"), "{msg}"),
                    (found, expected) => panic!("reader: {found:?} vs {expected:?}"),
                }
            }
        }
    }

    /// Compare every moment that should still exist.
    pub(super) fn check_past(&mut self) {
        self.db().check().unwrap();
        let latest = self.past.len() as u64 - 1;
        for txn in 0..=latest {
            let (state, time) = self.past[txn as usize].clone();
            for table in NAMES {
                let by_txn = format!("SELECT id, n FROM {table} AS OF TXN {txn} ORDER BY id");
                let found = self.db().execute(&by_txn);
                self.compare(txn, table, found, &state);
                // A primary key narrows both trees, and must find the same rows.
                let (one, after) = (7, 3);
                let by_key = format!(
                    "SELECT id, n FROM {table} AS OF TXN {txn} WHERE id > {after} ORDER BY id"
                );
                let found = self.db().execute(&by_key);
                let mut narrowed = state.clone();
                for rows in narrowed.values_mut() {
                    rows.retain(|id, _| *id > after);
                }
                self.compare(txn, table, found, &narrowed);
                let by_key = format!("SELECT id, n FROM {table} AS OF TXN {txn} WHERE id = {one}");
                let found = self.db().execute(&by_key);
                let mut narrowed = state.clone();
                for rows in narrowed.values_mut() {
                    rows.retain(|id, _| *id == one);
                }
                self.compare(txn, table, found, &narrowed);
                if txn > 0 {
                    let time = format_timestamp(time);
                    let by_time = format!("SELECT id, n FROM {table} AS OF '{time}' ORDER BY id");
                    let found = self.db().execute(&by_time);
                    self.compare(txn, table, found, &state);
                }
            }
        }
        self.check_history();
    }

    fn compare(&self, txn: u64, table: &str, found: Result<Output, Error>, state: &State) {
        match (found, model_rows(state, table)) {
            (Err(Error::Query(msg)), _) if txn < self.floor => {
                assert!(msg.contains("vacuumed"), "txn {txn}: {msg}")
            }
            (Ok(found), Some(expected)) if txn >= self.floor => {
                assert_eq!(pairs(&rows_of(found)), expected, "{table} at txn {txn}")
            }
            (Err(Error::Query(msg)), None) if txn >= self.floor => {
                assert!(msg.contains("no table"), "txn {txn}: {msg}")
            }
            (found, expected) => panic!("{table} at txn {txn}: {found:?} vs {expected:?}"),
        }
    }

    /// `HISTORY`'s stamps must rebuild each moment of a table that still exists.
    fn check_history(&mut self) {
        let latest = self.past.len() as u64 - 1;
        for table in NAMES {
            if !self.state.contains_key(table) {
                continue;
            }
            let sql = format!(
                "SELECT id, n, created_txn, deleted_txn FROM {table} HISTORY ORDER BY id, created_txn"
            );
            let versions = rows_of(self.db().execute(&sql).unwrap());
            // `HISTORY` shows this table, not an earlier one of the same name,
            // so only moments since it was made are compared.
            for txn in self.born[table].max(self.floor)..=latest {
                let txn_now = txn as i64;
                let found: Vec<Row> = versions
                    .iter()
                    .filter(|row| {
                        let Value::Int(created) = row[2] else {
                            panic!()
                        };
                        let live = match row[3] {
                            Value::Int(deleted) => deleted > txn_now,
                            _ => true,
                        };
                        created <= txn_now && live
                    })
                    .map(|row| pairs(std::slice::from_ref(row))[0])
                    .collect();
                let expected = model_rows(&self.past[txn as usize].0, table).unwrap_or_default();
                assert_eq!(found, expected, "HISTORY of {table} at txn {txn}");
            }
        }
    }
}
