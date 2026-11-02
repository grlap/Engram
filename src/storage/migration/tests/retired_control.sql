CREATE TABLE IF NOT EXISTS control_sessions (
                 session_id TEXT PRIMARY KEY,
                 project_id TEXT NOT NULL,
                 task_id TEXT NOT NULL REFERENCES control_anchors(task_id),
                 root_execution_id TEXT,
                 work_id TEXT,
                 run_id TEXT,
                 work_revision INTEGER,
                 claim_id TEXT,
                 claim_fence INTEGER,
                 routing_token TEXT NOT NULL,
                 actor_json BLOB NOT NULL,
                 bind_key TEXT NOT NULL,
                 bind_intent_hash TEXT NOT NULL,
                 bind_intent_json BLOB NOT NULL,
                 phase TEXT NOT NULL,
                 assurance TEXT NOT NULL,
                 mediated_effects_json TEXT NOT NULL,
                 confirmed_cursor INTEGER NOT NULL,
                 tentative_cursor INTEGER,
                 project_policy_epoch INTEGER NOT NULL,
                 task_admission_epoch INTEGER NOT NULL,
                 blocking_watermark INTEGER NOT NULL,
                 capability_map_revision INTEGER NOT NULL,
                 revision INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             ) STRICT;
             CREATE INDEX IF NOT EXISTS control_sessions_work_run
                 ON control_sessions(project_id, run_id, session_id);
             CREATE TABLE IF NOT EXISTS control_turn_results (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES control_sessions(session_id),
                 task_id TEXT NOT NULL REFERENCES control_anchors(task_id),
                 idempotency_key TEXT NOT NULL,
                 intent_hash TEXT NOT NULL,
                 intent_json BLOB NOT NULL,
                 decision_hash TEXT NOT NULL,
                 decision_json BLOB NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, idempotency_key)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_turn_grants (
                 grant_id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES control_sessions(session_id),
                 task_id TEXT NOT NULL REFERENCES control_anchors(task_id),
                 request_key TEXT NOT NULL,
                 grant_json BLOB NOT NULL,
                 state TEXT NOT NULL,
                 issued_at_ms INTEGER NOT NULL,
                 expires_at_ms INTEGER NOT NULL,
                 begun_at_ms INTEGER,
                 completed_at_ms INTEGER,
                 UNIQUE(session_id, request_key)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_turn_grant_supersessions (
                 superseded_grant_id TEXT PRIMARY KEY
                     REFERENCES control_turn_grants(grant_id),
                 session_id TEXT NOT NULL,
                 task_id TEXT NOT NULL REFERENCES control_anchors(task_id),
                 replacement_request_key TEXT NOT NULL,
                 replacement_decision_hash TEXT NOT NULL,
                 supersession_json BLOB NOT NULL,
                 superseded_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, replacement_request_key),
                 FOREIGN KEY(session_id, replacement_request_key)
                     REFERENCES control_turn_results(session_id, idempotency_key)
             ) STRICT;
