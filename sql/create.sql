CREATE TABLE Record (
    time  INTEGER NOT NULL,
    tid   INTEGER NOT NULL,
    tx    INTEGER NOT NULL,
    rx    INTEGER NOT NULL,
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

CREATE TABLE Month (
    uid   INTEGER NOT NULL PRIMARY KEY,
    sum   INTEGER NOT NULL
);

CREATE INDEX Record_time ON Record (time);
