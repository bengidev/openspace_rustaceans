-- PRD-03 Slice 11 — turn full-text search.
--
-- `turn_text_fts` mirrors searchable fields from `turns` using a
-- content-mirrored FTS5 table rather than external-content mode. The
-- mirrored columns are:
--
-- - `turn_id` — stable turn id returned by search.
-- - `chat_id` — parent chat id returned beside each hit.
-- - `kind` — serialized role/kind string.
-- - `payload_json` — complete Domain-serialized turn payload.
--
-- The table owns its indexed text so query-time ranking never has to
-- join back to `turns`; callers that only need ids can stay on the FTS
-- table. Three triggers keep the mirror in sync: INSERT writes the new
-- row, UPDATE rewrites it when any mirrored column changes, DELETE
-- removes it. The backfill strategy is the final INSERT...SELECT: when
-- this migration is applied to a v1 database that already has turns,
-- every existing row is copied once after trigger creation. Fresh v2+
-- writes then flow through the triggers.

CREATE VIRTUAL TABLE turn_text_fts USING fts5(
    turn_id UNINDEXED,
    chat_id UNINDEXED,
    kind,
    payload_json
);

CREATE TRIGGER trg_turn_text_fts_ai
AFTER INSERT ON turns
BEGIN
    INSERT INTO turn_text_fts (turn_id, chat_id, kind, payload_json)
    VALUES (new.id, new.chat_id, new.kind, new.payload_json);
END;

CREATE TRIGGER trg_turn_text_fts_au
AFTER UPDATE OF id, chat_id, kind, payload_json ON turns
BEGIN
    DELETE FROM turn_text_fts WHERE turn_id = old.id;
    INSERT INTO turn_text_fts (turn_id, chat_id, kind, payload_json)
    VALUES (new.id, new.chat_id, new.kind, new.payload_json);
END;

CREATE TRIGGER trg_turn_text_fts_ad
AFTER DELETE ON turns
BEGIN
    DELETE FROM turn_text_fts WHERE turn_id = old.id;
END;

INSERT INTO turn_text_fts (turn_id, chat_id, kind, payload_json)
SELECT id, chat_id, kind, payload_json
FROM turns;
