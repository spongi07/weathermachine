-- `report paper` reads the day-1 forecasts from the event journal. This small
-- partial index keeps that lookup off the journal's order-book rows.
CREATE INDEX IF NOT EXISTS event_journal_forecasts
    ON event_journal (available_at) WHERE kind = 'forecast_update';
