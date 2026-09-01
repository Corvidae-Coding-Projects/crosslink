CREATE TABLE issues (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    priority TEXT NOT NULL DEFAULT 'medium',
    parent_id INTEGER,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    closed_at TEXT,
    uuid TEXT,
    created_by TEXT
);

CREATE TABLE labels (
    issue_id INTEGER NOT NULL,
    label TEXT NOT NULL,
    PRIMARY KEY (issue_id, label),
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE TABLE dependencies (
    blocker_id INTEGER NOT NULL,
    blocked_id INTEGER NOT NULL,
    PRIMARY KEY (blocker_id, blocked_id),
    FOREIGN KEY (blocker_id) REFERENCES issues(id) ON DELETE CASCADE,
    FOREIGN KEY (blocked_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE TABLE comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id INTEGER NOT NULL,
    content TEXT NOT NULL,
    created_at TEXT NOT NULL,
    uuid TEXT,
    author TEXT,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    active_issue_id INTEGER,
    handoff_notes TEXT,
    last_action TEXT,
    agent_id TEXT,
    FOREIGN KEY (active_issue_id) REFERENCES issues(id) ON DELETE SET NULL
);

CREATE TABLE time_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    duration_seconds INTEGER,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE TABLE relations (
    issue_id_1 INTEGER NOT NULL,
    issue_id_2 INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (issue_id_1, issue_id_2),
    FOREIGN KEY (issue_id_1) REFERENCES issues(id) ON DELETE CASCADE,
    FOREIGN KEY (issue_id_2) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE TABLE milestones (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    created_at TEXT NOT NULL,
    closed_at TEXT,
    uuid TEXT
);

CREATE TABLE milestone_issues (
    milestone_id INTEGER NOT NULL,
    issue_id INTEGER NOT NULL,
    PRIMARY KEY (milestone_id, issue_id),
    FOREIGN KEY (milestone_id) REFERENCES milestones(id) ON DELETE CASCADE,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE INDEX idx_issues_status ON issues(status);

CREATE INDEX idx_issues_priority ON issues(priority);

CREATE INDEX idx_labels_issue ON labels(issue_id);

CREATE INDEX idx_comments_issue ON comments(issue_id);

CREATE INDEX idx_deps_blocker ON dependencies(blocker_id);

CREATE INDEX idx_deps_blocked ON dependencies(blocked_id);

CREATE INDEX idx_issues_parent ON issues(parent_id);

CREATE INDEX idx_time_entries_issue ON time_entries(issue_id);

CREATE INDEX idx_relations_1 ON relations(issue_id_1);

CREATE INDEX idx_relations_2 ON relations(issue_id_2);

CREATE INDEX idx_milestone_issues_m ON milestone_issues(milestone_id);

CREATE INDEX idx_milestone_issues_i ON milestone_issues(issue_id);

CREATE UNIQUE INDEX idx_issues_uuid ON issues(uuid);

CREATE UNIQUE INDEX idx_milestones_uuid ON milestones(uuid);

INSERT INTO issues (id, title, description, status, priority, parent_id, created_at, updated_at, closed_at, uuid, created_by) VALUES (1, 'fixture-v10', 'historical fixture', 'open', 'medium', NULL, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', NULL, '00000000-0000-0000-0000-000000000010', 'fixture-agent');

INSERT INTO labels (issue_id, label) VALUES (1, 'fixture');

INSERT INTO comments (id, issue_id, content, created_at, uuid, author) VALUES (1, 1, 'fixture comment', '2026-01-01T00:00:00Z', '10000000-0000-0000-0000-000000000010', 'fixture-agent');

INSERT INTO sessions (id, started_at, ended_at, active_issue_id, handoff_notes, last_action, agent_id) VALUES (1, '2026-01-01T00:00:00Z', NULL, 1, 'fixture handoff', 'fixture action', 'fixture-agent');

INSERT INTO time_entries (id, issue_id, started_at, ended_at, duration_seconds) VALUES (1, 1, '2026-01-01T00:00:00Z', '2026-01-01T00:01:00Z', 60);

INSERT INTO milestones (id, name, description, status, created_at, closed_at, uuid) VALUES (1, 'fixture milestone', 'historical fixture', 'open', '2026-01-01T00:00:00Z', NULL, '20000000-0000-0000-0000-000000000010');

INSERT INTO milestone_issues (milestone_id, issue_id) VALUES (1, 1);

PRAGMA user_version = 10;
