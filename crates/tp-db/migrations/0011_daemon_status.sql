-- What is ACTUALLY RUNNING, written by the daemon itself at startup.
--
-- Every other version signal describes a FILE: `tp --version` reports the
-- binary on disk, and the panel's own version is whatever bundle was last
-- installed. None of them can see the resident process, which on this project
-- routinely lags all of them — installing new binaries does not restart a
-- LaunchAgent, so the daemon serving peer requests can be arbitrarily old
-- while every file on disk looks current. That gap is invisible today and has
-- already caused confusion.
--
-- Written by tpd, read by the panel. Deliberately a single row: this is the
-- state of the one supervised daemon, not a history of runs. `CHECK (id = 1)`
-- makes a second row unrepresentable rather than merely unwritten, so a future
-- caller cannot quietly turn it into a log nobody prunes.
CREATE TABLE daemon_status (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  version     TEXT    NOT NULL,
  pid         INTEGER NOT NULL,
  started_at  INTEGER NOT NULL
) STRICT;
