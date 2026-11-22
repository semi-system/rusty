-- The history behind the statistics page (docs/design-queue.md): the delivery log is kept 30 days, the counts per day of a domain are kept 400. One row per domain, UTC day, direction and event, with
-- how many log lines there were. The garbage collector rolls the completed days of the last 28 days up from `mail_log` on every pass (an idempotent upsert) and removes rows older than 400 days.
CREATE TABLE message_stats (
  domain_id bigint NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  day       bigint NOT NULL CONSTRAINT message_stats_day_valid CHECK (day >= 0),
  direction text COLLATE "C" NOT NULL CONSTRAINT message_stats_direction_valid CHECK (direction IN ('out', 'in')),
  event     text COLLATE "C" NOT NULL CONSTRAINT message_stats_event_valid CHECK (length(event) BETWEEN 1 AND 32),
  count     bigint NOT NULL CONSTRAINT message_stats_count_valid CHECK (count >= 0),
  PRIMARY KEY (domain_id, day, direction, event)
);
-- The removal of rows past their retention.
CREATE INDEX message_stats_day ON message_stats (day);
-- Additive: a binary of the previous schema never reads the table.
INSERT INTO schema_compat (version, min_binary) VALUES (44, 43);
