-- Weather Machine initial schema.
--
-- Conventions
--   * All timestamps are TIMESTAMPTZ (UTC).
--   * Money, prices and share quantities are exact integers in micro-units
--     (*_micros, 6 decimals — matches on-chain USDC/pUSD and CTF decimals).
--   * Temperatures are integers in tenths of °C (*_dc).
--   * Raw provider data is never overwritten; observations are versioned.

-- ---------------------------------------------------------------------------
-- Reference data
-- ---------------------------------------------------------------------------
CREATE TABLE stations (
    station_id      TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    wmo_id          TEXT,
    latitude        DOUBLE PRECISION,
    longitude       DOUBLE PRECISION,
    elevation_m     DOUBLE PRECISION,
    timezone        TEXT NOT NULL,
    metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE locations (
    location_id     TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    station_id      TEXT NOT NULL REFERENCES stations(station_id),
    timezone        TEXT NOT NULL,
    config          JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Provider access audit and raw payloads
-- ---------------------------------------------------------------------------
CREATE TABLE provider_requests (
    id              BIGSERIAL PRIMARY KEY,
    provider        TEXT NOT NULL,
    endpoint        TEXT NOT NULL,
    station_id      TEXT,
    requested_at    TIMESTAMPTZ NOT NULL,
    completed_at    TIMESTAMPTZ NOT NULL,
    status          INTEGER,
    latency_ms      BIGINT NOT NULL,
    bytes           BIGINT NOT NULL,
    cache           TEXT NOT NULL,
    retry_count     INTEGER NOT NULL DEFAULT 0,
    throttled       BOOLEAN NOT NULL DEFAULT false,
    error_class     TEXT,
    gate_wait_ms    BIGINT NOT NULL DEFAULT 0,
    payload_sha256  TEXT
);
CREATE INDEX provider_requests_provider_time ON provider_requests (provider, requested_at DESC);

-- Identical payloads are stored once per provider; every sighting is counted.
CREATE TABLE raw_weather_payloads (
    id              BIGSERIAL PRIMARY KEY,
    provider        TEXT NOT NULL,
    station_id      TEXT,
    endpoint        TEXT NOT NULL,
    sha256          TEXT NOT NULL,
    first_fetched_at TIMESTAMPTZ NOT NULL,
    last_fetched_at TIMESTAMPTZ NOT NULL,
    seen_count      BIGINT NOT NULL DEFAULT 1,
    status          INTEGER NOT NULL,
    content_type    TEXT,
    body            BYTEA NOT NULL,
    parser_version  INTEGER NOT NULL,
    UNIQUE (provider, sha256)
);

-- ---------------------------------------------------------------------------
-- Observations (append-only, versioned)
-- ---------------------------------------------------------------------------
CREATE TABLE weather_observations (
    id                  BIGSERIAL PRIMARY KEY,
    station_id          TEXT NOT NULL,
    observed_at         TIMESTAMPTZ NOT NULL,
    report_type         TEXT NOT NULL CHECK (report_type IN ('METAR', 'SPECI')),
    version             INTEGER NOT NULL CHECK (version >= 1),
    temperature_dc      INTEGER,
    dewpoint_dc         INTEGER,
    precision           TEXT NOT NULL,
    raw_text            TEXT NOT NULL,
    content_hash        TEXT NOT NULL,
    provider            TEXT NOT NULL,
    provider_receipt_at TIMESTAMPTZ,
    fetched_at          TIMESTAMPTZ NOT NULL,
    ingested_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    parser_version      INTEGER NOT NULL,
    dedup_class         TEXT NOT NULL,
    quality             JSONB NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (station_id, observed_at, report_type, version)
);
CREATE INDEX weather_observations_station_time ON weather_observations (station_id, observed_at DESC);

-- Latest version of every observation.
CREATE VIEW weather_observations_current AS
SELECT DISTINCT ON (station_id, observed_at, report_type) *
FROM weather_observations
ORDER BY station_id, observed_at, report_type, version DESC;

CREATE TABLE weather_corrections (
    id                      BIGSERIAL PRIMARY KEY,
    station_id              TEXT NOT NULL,
    observed_at             TIMESTAMPTZ NOT NULL,
    report_type             TEXT NOT NULL,
    previous_version        INTEGER NOT NULL,
    current_version         INTEGER NOT NULL,
    previous_temperature_dc INTEGER,
    current_temperature_dc  INTEGER,
    labeled                 BOOLEAN NOT NULL,
    detected_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE forecast_snapshots (
    id                  BIGSERIAL PRIMARY KEY,
    location_id         TEXT NOT NULL,
    provider            TEXT NOT NULL,
    model               TEXT NOT NULL,
    issued_at           TIMESTAMPTZ NOT NULL,
    available_at        TIMESTAMPTZ NOT NULL,
    predicted_max_dc    INTEGER,
    hourly              JSONB NOT NULL DEFAULT '[]'::jsonb,
    raw                 JSONB
);
CREATE INDEX forecast_snapshots_location_issue ON forecast_snapshots (location_id, issued_at DESC);

-- ---------------------------------------------------------------------------
-- Markets
-- ---------------------------------------------------------------------------
CREATE TABLE market_rules (
    rules_sha256        TEXT PRIMARY KEY,
    text                TEXT NOT NULL,
    resolution_source_url TEXT,
    parsed_spec         JSONB NOT NULL,
    review_status       TEXT NOT NULL DEFAULT 'auto_parsed',
    reviewed_by         TEXT,
    reviewed_at         TIMESTAMPTZ,
    first_seen_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE markets (
    event_slug          TEXT PRIMARY KEY,
    event_id            TEXT NOT NULL,
    location_id         TEXT NOT NULL,
    station_id          TEXT NOT NULL,
    local_date          DATE NOT NULL,
    extreme             TEXT NOT NULL,
    unit                TEXT NOT NULL,
    neg_risk            BOOLEAN NOT NULL,
    title               TEXT NOT NULL,
    end_time            TIMESTAMPTZ,
    active              BOOLEAN NOT NULL,
    closed              BOOLEAN NOT NULL,
    rules_sha256        TEXT NOT NULL REFERENCES market_rules(rules_sha256),
    taker_fee_rate_micros INTEGER NOT NULL,
    discovered_at       TIMESTAMPTZ NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_value      INTEGER,
    resolved_at         TIMESTAMPTZ
);
CREATE INDEX markets_location_date ON markets (location_id, local_date);

CREATE TABLE market_outcomes (
    condition_id        TEXT PRIMARY KEY,
    event_slug          TEXT NOT NULL REFERENCES markets(event_slug) ON DELETE CASCADE,
    question_id         TEXT,
    label               TEXT NOT NULL,
    bucket_lower        INTEGER,
    bucket_upper        INTEGER,
    unit                TEXT NOT NULL,
    yes_token           TEXT NOT NULL UNIQUE,
    no_token            TEXT NOT NULL UNIQUE,
    tick_size_micros    INTEGER NOT NULL,
    min_order_size_micros BIGINT NOT NULL,
    accepting_orders    BOOLEAN NOT NULL,
    closed              BOOLEAN NOT NULL
);

-- Verbatim metadata responses (e.g. Gamma JSON), deduplicated by hash.
CREATE TABLE market_snapshots (
    id                  BIGSERIAL PRIMARY KEY,
    event_slug          TEXT NOT NULL,
    captured_at         TIMESTAMPTZ NOT NULL,
    sha256              TEXT NOT NULL,
    payload             BYTEA NOT NULL,
    UNIQUE (event_slug, sha256)
);

-- Our own recording of order books (historical books are not available from the venue).
CREATE TABLE orderbook_snapshots (
    id                  BIGSERIAL PRIMARY KEY,
    token_id            TEXT NOT NULL,
    captured_at         TIMESTAMPTZ NOT NULL,
    exchange_ts         TIMESTAMPTZ,
    best_bid_micros     INTEGER,
    best_ask_micros     INTEGER,
    bid_size_micros     BIGINT,
    ask_size_micros     BIGINT,
    levels              JSONB NOT NULL,
    hash                TEXT
);
CREATE INDEX orderbook_snapshots_token_time ON orderbook_snapshots (token_id, captured_at);

CREATE TABLE market_trades (
    id                  BIGSERIAL PRIMARY KEY,
    token_id            TEXT NOT NULL,
    ts                  TIMESTAMPTZ NOT NULL,
    price_micros        INTEGER NOT NULL,
    size_micros         BIGINT NOT NULL,
    aggressor           TEXT
);
CREATE INDEX market_trades_token_time ON market_trades (token_id, ts);

-- ---------------------------------------------------------------------------
-- State, signals and decisions
-- ---------------------------------------------------------------------------
CREATE TABLE strategy_runs (
    run_id              UUID PRIMARY KEY,
    mode                TEXT NOT NULL,
    started_at          TIMESTAMPTZ NOT NULL,
    ended_at            TIMESTAMPTZ,
    model_id            TEXT NOT NULL,
    version             TEXT NOT NULL,
    config              JSONB NOT NULL
);

CREATE TABLE temperature_states (
    id                  BIGSERIAL PRIMARY KEY,
    run_id              UUID NOT NULL,
    station_id          TEXT NOT NULL,
    local_date          DATE NOT NULL,
    view                TEXT NOT NULL,
    computed_at         TIMESTAMPTZ NOT NULL,
    state               JSONB NOT NULL
);

CREATE TABLE peak_candidates (
    id                  BIGSERIAL PRIMARY KEY,
    run_id              UUID NOT NULL,
    station_id          TEXT NOT NULL,
    local_date          DATE NOT NULL,
    view                TEXT NOT NULL,
    high_dc             INTEGER NOT NULL,
    first_at            TIMESTAMPTZ NOT NULL,
    last_at             TIMESTAMPTZ NOT NULL,
    windows_met         INTEGER[] NOT NULL,
    recorded_at         TIMESTAMPTZ NOT NULL
);

CREATE TABLE decision_snapshots (
    run_id              UUID NOT NULL,
    decision_id         BIGINT NOT NULL,
    strategy            TEXT NOT NULL,
    at                  TIMESTAMPTZ NOT NULL,
    location_id         TEXT NOT NULL,
    event_slug          TEXT,
    summary             TEXT NOT NULL,
    inputs              JSONB NOT NULL,
    outputs             JSONB NOT NULL,
    approved            BOOLEAN NOT NULL,
    reasons             TEXT[] NOT NULL,
    PRIMARY KEY (run_id, decision_id)
);
CREATE INDEX decision_snapshots_time ON decision_snapshots (at DESC);

CREATE TABLE signals (
    id                  BIGSERIAL PRIMARY KEY,
    run_id              UUID NOT NULL,
    decision_id         BIGINT NOT NULL,
    strategy            TEXT NOT NULL,
    event_slug          TEXT NOT NULL,
    bucket_label        TEXT NOT NULL,
    outcome_side        TEXT NOT NULL,
    p_win               DOUBLE PRECISION NOT NULL,
    ev_per_share        DOUBLE PRECISION NOT NULL,
    break_even          DOUBLE PRECISION NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL
);

-- ---------------------------------------------------------------------------
-- Orders, fills, positions
-- ---------------------------------------------------------------------------
CREATE TABLE orders (
    client_order_id     TEXT PRIMARY KEY,
    run_id              UUID NOT NULL,
    decision_id         BIGINT NOT NULL,
    strategy            TEXT NOT NULL,
    location_id         TEXT NOT NULL,
    event_slug          TEXT NOT NULL,
    token_id            TEXT NOT NULL,
    condition_id        TEXT NOT NULL,
    outcome_side        TEXT NOT NULL,
    side                TEXT NOT NULL,
    kind                TEXT NOT NULL,
    limit_price_micros  INTEGER NOT NULL,
    shares_micros       BIGINT NOT NULL,
    tif                 JSONB NOT NULL,
    status              TEXT NOT NULL,
    filled_micros       BIGINT NOT NULL,
    avg_price_micros    INTEGER,
    fees_micros         BIGINT NOT NULL,
    venue_order_id      TEXT,
    reason              TEXT,
    created_at          TIMESTAMPTZ NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL
);
CREATE INDEX orders_run ON orders (run_id, created_at);

CREATE TABLE fills (
    id                  BIGSERIAL PRIMARY KEY,
    client_order_id     TEXT NOT NULL REFERENCES orders(client_order_id),
    token_id            TEXT NOT NULL,
    side                TEXT NOT NULL,
    price_micros        INTEGER NOT NULL,
    shares_micros       BIGINT NOT NULL,
    fee_micros          BIGINT NOT NULL,
    liquidity           TEXT NOT NULL,
    ts                  TIMESTAMPTZ NOT NULL
);

CREATE TABLE positions (
    run_id              UUID NOT NULL,
    token_id            TEXT NOT NULL,
    event_slug          TEXT NOT NULL,
    outcome_side        TEXT NOT NULL,
    bucket_label        TEXT NOT NULL,
    shares_micros       BIGINT NOT NULL,
    cost_basis_micros   BIGINT NOT NULL,
    realized_pnl_micros BIGINT NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (run_id, token_id)
);

-- ---------------------------------------------------------------------------
-- Backtests
-- ---------------------------------------------------------------------------
CREATE TABLE backtest_runs (
    run_id              UUID PRIMARY KEY,
    from_date           DATE NOT NULL,
    to_date             DATE NOT NULL,
    fidelity            TEXT NOT NULL,
    parameters          JSONB NOT NULL,
    metrics             JSONB NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE backtest_trades (
    id                  BIGSERIAL PRIMARY KEY,
    run_id              UUID NOT NULL REFERENCES backtest_runs(run_id) ON DELETE CASCADE,
    decision_id         BIGINT NOT NULL,
    event_slug          TEXT NOT NULL,
    token_id            TEXT NOT NULL,
    side                TEXT NOT NULL,
    price_micros        INTEGER NOT NULL,
    shares_micros       BIGINT NOT NULL,
    fee_micros          BIGINT NOT NULL,
    pnl_micros          BIGINT,
    opened_at           TIMESTAMPTZ NOT NULL,
    closed_at           TIMESTAMPTZ
);

-- ---------------------------------------------------------------------------
-- Health, system events and the durable event journal
-- ---------------------------------------------------------------------------
CREATE TABLE provider_health_events (
    id                  BIGSERIAL PRIMARY KEY,
    provider            TEXT NOT NULL,
    station_id          TEXT,
    state               TEXT NOT NULL,
    previous_state      TEXT,
    reason              TEXT NOT NULL,
    snapshot            JSONB NOT NULL,
    at                  TIMESTAMPTZ NOT NULL
);
CREATE INDEX provider_health_events_time ON provider_health_events (provider, at DESC);

CREATE TABLE system_events (
    id                  BIGSERIAL PRIMARY KEY,
    at                  TIMESTAMPTZ NOT NULL DEFAULT now(),
    level               TEXT NOT NULL,
    kind                TEXT NOT NULL,
    message             TEXT NOT NULL,
    details             JSONB NOT NULL DEFAULT '{}'::jsonb
);

-- Every engine input, in sequence, for crash recovery and exact replay.
CREATE TABLE event_journal (
    run_id              UUID NOT NULL,
    seq                 BIGINT NOT NULL,
    available_at        TIMESTAMPTZ NOT NULL,
    recorded_at         TIMESTAMPTZ NOT NULL,
    source              TEXT NOT NULL,
    kind                TEXT NOT NULL,
    payload             JSONB NOT NULL,
    PRIMARY KEY (run_id, seq)
);
CREATE INDEX event_journal_time ON event_journal (available_at);
