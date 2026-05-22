-- PRD-03 Slice 6 — initial schema.
--
-- Lays down every table the repository slices that follow this one
-- (workspaces, sessions, chats, turns, attachments, permission grants,
-- recent workspaces) read and write through. The schema is deliberately
-- conservative: TEXT for IDs (UUIDs serialised as strings), INTEGER for
-- unix-epoch timestamps, JSON-as-TEXT for blobs the Domain types
-- already serialise themselves. SQLite type affinity makes the column
-- declarations advisory rather than enforced, so the runtime types are
-- whatever the repository impls write — keep the Domain types in
-- lockstep with this file.
--
-- The FTS5 virtual table that mirrors `turns.payload_json` and the
-- triggers that keep it in sync land in a follow-up slice (PRD-03
-- Slice 11). Wiring them here would block the parallel repository
-- slices on a feature only the chat search path consumes.
--
-- Append-only by convention: never edit this file once it ships. The
-- migration runner replays it on every empty database, and a shipped
-- edit would silently diverge from databases that already applied the
-- old version.

-- ────────────────────────────────────────────────────────────────────
-- workspaces — top-level project roots the user has opened.
-- ────────────────────────────────────────────────────────────────────
CREATE TABLE workspaces (
    id              TEXT    PRIMARY KEY,
    root_path       TEXT    NOT NULL,
    last_opened_at  INTEGER NOT NULL,
    trust_mode      TEXT    NOT NULL,
    created_at      INTEGER NOT NULL
);

CREATE INDEX idx_workspaces_last_opened
    ON workspaces (last_opened_at DESC);

-- ────────────────────────────────────────────────────────────────────
-- sessions — workspace-scoped layouts (pane tree, active pane, name).
-- `layout_blob` carries a Domain-serialised JSON snapshot of the pane
-- tree; the repository never inspects it column-side.
-- ────────────────────────────────────────────────────────────────────
CREATE TABLE sessions (
    id              TEXT    PRIMARY KEY,
    workspace_id    TEXT    NOT NULL,
    name            TEXT    NOT NULL,
    mode            TEXT    NOT NULL,
    layout_blob     TEXT    NOT NULL,
    active_pane_id  TEXT,
    updated_at      INTEGER NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE CASCADE
);

CREATE INDEX idx_sessions_workspace
    ON sessions (workspace_id, updated_at DESC);

-- ────────────────────────────────────────────────────────────────────
-- chats — conversation metadata. Turn content lives in `turns`; the
-- `head_turn_id` pointer is the leaf the agent loop appends after
-- (branching is layered on top of `turns.parent_turn_id` later — PRD-15).
-- ────────────────────────────────────────────────────────────────────
CREATE TABLE chats (
    id              TEXT    PRIMARY KEY,
    workspace_id    TEXT    NOT NULL,
    title           TEXT    NOT NULL,
    model_provider  TEXT    NOT NULL,
    model_id        TEXT    NOT NULL,
    system_prompt   TEXT,
    head_turn_id    TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE CASCADE
);

CREATE INDEX idx_chats_workspace
    ON chats (workspace_id, updated_at DESC);

-- ────────────────────────────────────────────────────────────────────
-- turns — append-only conversation log with `parent_turn_id` for
-- branching. `sequence` orders siblings inside a branch so the chat
-- view can render without a recursive walk on the hot path.
-- ────────────────────────────────────────────────────────────────────
CREATE TABLE turns (
    id              TEXT    PRIMARY KEY,
    chat_id         TEXT    NOT NULL,
    parent_turn_id  TEXT,
    kind            TEXT    NOT NULL,
    payload_json    TEXT    NOT NULL,
    sequence        INTEGER NOT NULL,
    created_at      INTEGER NOT NULL,
    FOREIGN KEY (chat_id)        REFERENCES chats (id) ON DELETE CASCADE,
    FOREIGN KEY (parent_turn_id) REFERENCES turns (id) ON DELETE SET NULL
);

CREATE INDEX idx_turns_chat_sequence
    ON turns (chat_id, sequence);

CREATE INDEX idx_turns_parent
    ON turns (parent_turn_id);

-- ────────────────────────────────────────────────────────────────────
-- attachments — binary blobs attached to a turn (image input, file
-- upload, captured screenshot). The `data_blob` column is BLOB; the
-- mime + kind columns let the renderer pick a viewer without parsing
-- the bytes first.
-- ────────────────────────────────────────────────────────────────────
CREATE TABLE attachments (
    id          TEXT    PRIMARY KEY,
    turn_id     TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    mime        TEXT    NOT NULL,
    data_blob   BLOB    NOT NULL,
    created_at  INTEGER NOT NULL,
    FOREIGN KEY (turn_id) REFERENCES turns (id) ON DELETE CASCADE
);

CREATE INDEX idx_attachments_turn
    ON attachments (turn_id);

-- ────────────────────────────────────────────────────────────────────
-- permission_grants — TOFU grants per workspace × tool × network
-- pattern. Composite primary key models "the same triple is granted
-- exactly once"; a re-grant is an upsert, not a duplicate row.
-- ────────────────────────────────────────────────────────────────────
CREATE TABLE permission_grants (
    workspace_id     TEXT    NOT NULL,
    tool_id          TEXT    NOT NULL,
    network_pattern  TEXT    NOT NULL,
    granted_at       INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, tool_id, network_pattern),
    FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE CASCADE
);

-- ────────────────────────────────────────────────────────────────────
-- recent_workspaces — denormalised most-recently-used list the home
-- shell renders without scanning every workspace row.
-- ────────────────────────────────────────────────────────────────────
CREATE TABLE recent_workspaces (
    workspace_id    TEXT    PRIMARY KEY,
    last_opened_at  INTEGER NOT NULL,
    FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE CASCADE
);

CREATE INDEX idx_recent_workspaces_last_opened
    ON recent_workspaces (last_opened_at DESC);
