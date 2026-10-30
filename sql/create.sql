CREATE TABLE Record (
    time  INTEGER NOT NULL,
    tid   INTEGER NOT NULL,
    tx    INTEGER NOT NULL,
    rx    INTEGER NOT NULL,
    -- The index of this key starts with time, so it serves the time range queries.
    PRIMARY KEY (time, tid)
);

CREATE TABLE Last (
    time  INTEGER NOT NULL,
    tid   INTEGER NOT NULL PRIMARY KEY
);

CREATE TABLE Meta (
    id    INTEGER NOT NULL PRIMARY KEY CHECK (id = 0),
    time  INTEGER NOT NULL
);
