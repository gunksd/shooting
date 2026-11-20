//! SQLite persistence. One writer (the engine); WAL mode so readers (CLI) do not block it.
use alloy::primitives::B256;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;

pub const SCHEMA: &str = include_str!("../schema.sql");

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRow {
    pub tx_hash: B256,
    pub task_id: B256,
    pub nonce: u64,
    pub raw_tx: Vec<u8>,
    pub gas_limit: u64,
    pub gas_price: u128,
    pub max_cost: u128,
    pub state: String,
    pub channels: String,
    pub submitted_block: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub task_id: B256,
    pub state: String,
    pub cap_wei: u128,
    pub spent_wei: u128,
    pub attempts: u32,
    pub armed_block: Option<u64>,
    pub open_block: Option<u64>,
    pub open_tx_index: Option<u64>,
}

fn u128s(v: u128) -> String {
    v.to_string()
}

impl Store {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn upsert_task(&self, id: B256, spec_json: &str, cap_wei: u128, now_ms: u64) -> Result<()> {
        self.c().execute(
            "INSERT INTO tasks(task_id, spec_json, state, cap_wei, spent_wei, attempts, created_ms, updated_ms)
             VALUES(?1, ?2, 'ready', ?3, '0', 0, ?4, ?4)
             ON CONFLICT(task_id) DO UPDATE SET spec_json = excluded.spec_json, updated_ms = excluded.updated_ms",
            params![id.to_string(), spec_json, u128s(cap_wei), now_ms],
        )?;
        Ok(())
    }

    pub fn task(&self, id: B256) -> Result<Option<TaskRow>> {
        Ok(self
            .c()
            .query_row(
                "SELECT state, cap_wei, spent_wei, attempts, armed_block, open_block, open_tx_index FROM tasks WHERE task_id = ?1",
                params![id.to_string()],
                |r| {
                    Ok(TaskRow {
                        task_id: id,
                        state: r.get(0)?,
                        cap_wei: r.get::<_, String>(1)?.parse().unwrap_or(0),
                        spent_wei: r.get::<_, String>(2)?.parse().unwrap_or(0),
                        attempts: r.get(3)?,
                        armed_block: r.get(4)?,
                        open_block: r.get(5)?,
                        open_tx_index: r.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    /// Compare-and-set on the task state. Returns true only for the caller that made the change, which
    /// is what keeps competing triggers (several nodes, duplicate events) from firing twice.
    pub fn transition(&self, id: B256, from: &[&str], to: &str, now_ms: u64) -> Result<bool> {
        let list = from
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(",");
        let n = self.c().execute(
            &format!("UPDATE tasks SET state = ?1, updated_ms = ?2 WHERE task_id = ?3 AND state IN ({list})"),
            params![to, now_ms, id.to_string()],
        )?;
        Ok(n == 1)
    }

    pub fn set_open(&self, id: B256, block: u64, tx_index: u64, tx_hash: B256) -> Result<()> {
        self.c().execute(
            "UPDATE tasks SET open_block = ?1, open_tx_index = ?2, open_tx_hash = ?3 WHERE task_id = ?4",
            params![block, tx_index, tx_hash.to_string(), id.to_string()],
        )?;
        Ok(())
    }

    pub fn set_armed(&self, id: B256, block: Option<u64>) -> Result<()> {
        self.c().execute(
            "UPDATE tasks SET armed_block = ?1 WHERE task_id = ?2",
            params![block, id.to_string()],
        )?;
        Ok(())
    }

    pub fn set_spent(&self, id: B256, spent: u128) -> Result<()> {
        self.c().execute(
            "UPDATE tasks SET spent_wei = ?1 WHERE task_id = ?2",
            params![u128s(spent), id.to_string()],
        )?;
        Ok(())
    }

    pub fn set_result(&self, id: B256, json: &str) -> Result<()> {
        self.c().execute(
            "UPDATE tasks SET result_json = ?1 WHERE task_id = ?2",
            params![json, id.to_string()],
        )?;
        Ok(())
    }

    /// Recorded before anything is handed to a channel, so a crash between "sent" and "saved" cannot
    /// lose track of a transaction that may land.
    pub fn insert_attempt(&self, a: &AttemptRow, now_ms: u64) -> Result<()> {
        let c = self.c();
        c.execute(
            "INSERT INTO attempts(tx_hash, task_id, nonce, raw_tx, gas_limit, gas_price, max_cost, state, channels, submitted_block, created_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![a.tx_hash.to_string(), a.task_id.to_string(), a.nonce, a.raw_tx, a.gas_limit, u128s(a.gas_price), u128s(a.max_cost), a.state, a.channels, a.submitted_block, now_ms],
        )?;
        c.execute(
            "UPDATE tasks SET attempts = attempts + 1 WHERE task_id = ?1",
            params![a.task_id.to_string()],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_attempt(
        &self,
        hash: B256,
        state: &str,
        block: Option<u64>,
        block_hash: Option<B256>,
        tx_index: Option<u64>,
        gas_used: Option<u64>,
        cost: u128,
        detail: &str,
    ) -> Result<()> {
        self.c().execute(
            "UPDATE attempts SET state = ?1, block_number = ?2, block_hash = ?3, tx_index = ?4, gas_used = ?5, cost_wei = ?6, detail = ?7 WHERE tx_hash = ?8",
            params![state, block, block_hash.map(|h| h.to_string()), tx_index, gas_used, u128s(cost), detail, hash.to_string()],
        )?;
        Ok(())
    }

    pub fn attempts_in_state(&self, states: &[&str]) -> Result<Vec<AttemptRow>> {
        let list = states
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(",");
        let c = self.c();
        let mut st = c.prepare(&format!(
            "SELECT tx_hash, task_id, nonce, raw_tx, gas_limit, gas_price, max_cost, state, channels, submitted_block FROM attempts WHERE state IN ({list}) ORDER BY nonce"
        ))?;
        let rows = st.query_map([], |r| {
            Ok(AttemptRow {
                tx_hash: r.get::<_, String>(0)?.parse().unwrap_or_default(),
                task_id: r.get::<_, String>(1)?.parse().unwrap_or_default(),
                nonce: r.get(2)?,
                raw_tx: r.get(3)?,
                gas_limit: r.get(4)?,
                gas_price: r.get::<_, String>(5)?.parse().unwrap_or(0),
                max_cost: r.get::<_, String>(6)?.parse().unwrap_or(0),
                state: r.get(7)?,
                channels: r.get(8)?,
                submitted_block: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn metric(&self, task: B256, name: &str, value: i64, now_ms: u64) -> Result<()> {
        self.c().execute(
            "INSERT INTO metrics(task_id, name, value, at_ms) VALUES(?1,?2,?3,?4)",
            params![task.to_string(), name, value, now_ms],
        )?;
        Ok(())
    }

    pub fn metrics(&self, task: B256) -> Result<Vec<(String, i64)>> {
        let c = self.c();
        let mut st =
            c.prepare("SELECT name, value FROM metrics WHERE task_id = ?1 ORDER BY rowid")?;
        let rows = st.query_map(params![task.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn add_init_cost(
        &self,
        task: B256,
        label: &str,
        cost_wei: u128,
        tx_hash: &str,
    ) -> Result<()> {
        self.c().execute(
            "INSERT INTO init_costs(task_id, label, cost_wei, tx_hash) VALUES(?1,?2,?3,?4)",
            params![task.to_string(), label, u128s(cost_wei), tx_hash],
        )?;
        Ok(())
    }

    pub fn init_cost_total(&self, task: B256) -> Result<u128> {
        let c = self.c();
        let mut st = c.prepare("SELECT cost_wei FROM init_costs WHERE task_id = ?1")?;
        let rows = st.query_map(params![task.to_string()], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()?.parse::<u128>().ok()).sum())
    }

    pub fn task_result(&self, id: B256) -> Result<Option<String>> {
        Ok(self
            .c()
            .query_row(
                "SELECT result_json FROM tasks WHERE task_id = ?1",
                params![id.to_string()],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn kv_set(&self, k: &str, v: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO kv(k, v) VALUES(?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![k, v],
        )?;
        Ok(())
    }

    pub fn kv_get(&self, k: &str) -> Result<Option<String>> {
        Ok(self
            .c()
            .query_row("SELECT v FROM kv WHERE k = ?1", params![k], |r| r.get(0))
            .optional()?)
    }
}
