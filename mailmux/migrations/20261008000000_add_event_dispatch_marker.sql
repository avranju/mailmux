ALTER TABLE events ADD COLUMN dispatched_at TIMESTAMPTZ;

UPDATE events e
SET dispatched_at = now()
WHERE EXISTS (SELECT 1 FROM processor_jobs pj WHERE pj.event_id = e.id);

CREATE INDEX idx_events_undispatched_id ON events(id) WHERE dispatched_at IS NULL;
