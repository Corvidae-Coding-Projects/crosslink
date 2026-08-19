CREATE TABLE issues (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    priority TEXT NOT NULL DEFAULT 'medium',
    parent_id INTEGER,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    closed_at TEXT
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
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    active_issue_id INTEGER,
    handoff_notes TEXT,
    FOREIGN KEY (active_issue_id) REFERENCES issues(id)
);

CREATE INDEX idx_issues_status ON issues(status);

CREATE INDEX idx_issues_priority ON issues(priority);

CREATE INDEX idx_labels_issue ON labels(issue_id);

CREATE INDEX idx_comments_issue ON comments(issue_id);

CREATE INDEX idx_deps_blocker ON dependencies(blocker_id);

CREATE INDEX idx_deps_blocked ON dependencies(blocked_id);

CREATE INDEX idx_issues_parent ON issues(parent_id);

INSERT INTO issues (id, title, description, status, priority, parent_id, created_at, updated_at, closed_at) VALUES (1, 'fixture-v02', 'historical fixture', 'open', 'medium', NULL, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', NULL);

INSERT INTO labels (issue_id, label) VALUES (1, 'fixture');

INSERT INTO comments (id, issue_id, content, created_at) VALUES (1, 1, 'fixture comment', '2026-01-01T00:00:00Z');

INSERT INTO sessions (id, started_at, ended_at, active_issue_id, handoff_notes) VALUES (1, '2026-01-01T00:00:00Z', NULL, 1, 'fixture handoff');

PRAGMA user_version = 2;
