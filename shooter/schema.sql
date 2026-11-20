-- Task state: ready -> armed -> triggered -> submitted -> confirmed
--             any open state -> stopped | cancelled | expired
CREATE TABLE IF NOT EXISTS tasks (
  task_id       TEXT PRIMARY KEY,           -- on-chain taskId (bytes32 hex)
  spec_json     TEXT NOT NULL,
  state         TEXT NOT NULL,
  cap_wei       TEXT NOT NULL,              -- execution budget fixed in wei when the task was armed
  spent_wei     TEXT NOT NULL,              -- confirmed execution costs (successful and failed)
  attempts      INTEGER NOT NULL DEFAULT 0, -- transactions handed to channels
  armed_block   INTEGER,
  open_block    INTEGER,                    -- block of the launch transaction
  open_tx_index INTEGER,
  open_tx_hash  TEXT,
  result_json   TEXT,
  created_ms    INTEGER NOT NULL,
  updated_ms    INTEGER NOT NULL
);

-- One row per signed transaction. state: signed | sent | unknown | included_ok | included_failed | replaced | final_ok
CREATE TABLE IF NOT EXISTS attempts (
  tx_hash         TEXT PRIMARY KEY,
  task_id         TEXT NOT NULL REFERENCES tasks(task_id),
  nonce           INTEGER NOT NULL,
  raw_tx          BLOB NOT NULL,
  gas_limit       INTEGER NOT NULL,
  gas_price       TEXT NOT NULL,
  max_cost        TEXT NOT NULL,
  state           TEXT NOT NULL,
  channels        TEXT NOT NULL,
  submitted_block INTEGER NOT NULL,
  block_number    INTEGER,
  block_hash      TEXT,
  tx_index        INTEGER,
  gas_used        INTEGER,
  cost_wei        TEXT,
  detail          TEXT,
  created_ms      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS attempts_state ON attempts(state);

-- Deployment / approve / createTask costs, kept apart from the execution budget.
CREATE TABLE IF NOT EXISTS init_costs (
  task_id  TEXT NOT NULL,
  label    TEXT NOT NULL,
  cost_wei TEXT NOT NULL,
  tx_hash  TEXT NOT NULL
);

-- Measured timings and outcomes (ms or plain integers), never estimates.
CREATE TABLE IF NOT EXISTS metrics (
  task_id TEXT NOT NULL,
  name    TEXT NOT NULL,
  value   INTEGER NOT NULL,
  at_ms   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);
