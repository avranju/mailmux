-- Add nullable JSONB output column to processor_jobs.
-- Existing rows receive NULL; rows without a ProcessorOutput remain NULL.
ALTER TABLE processor_jobs ADD COLUMN output JSONB;
