CREATE TABLE projects
(
    id         INTEGER PRIMARY KEY, -- GitHub repository ID
    owner      TEXT      NOT NULL,  -- GitHub repository owner
    name       TEXT      NOT NULL,  -- GitHub repository name
    created_at TIMESTAMP NOT NULL,
    updated_at TIMESTAMP NOT NULL
);

CREATE TABLE reports
(
    id         INTEGER PRIMARY KEY,
    project_id INTEGER  NOT NULL, -- GitHub repository ID
    version    TEXT     NOT NULL, -- Game ID
    git_commit TEXT     NOT NULL, -- Git commit SHA
    timestamp  DATETIME NOT NULL, -- Git commit timestamp
    data       BLOB     NOT NULL, -- Serialized report data
    FOREIGN KEY (project_id) REFERENCES projects (id)
);

CREATE UNIQUE INDEX reports_project_id_version_git_commit_index ON reports (project_id, version, git_commit);
CREATE INDEX reports_timestamp_index ON reports (timestamp);

CREATE TABLE report_report_units
(
    report_id      INTEGER NOT NULL,
    report_unit_id BLOB    NOT NULL,
    unit_index     INTEGER NOT NULL, -- Index of the report unit in the report
    PRIMARY KEY (report_id, report_unit_id),
    FOREIGN KEY (report_id) REFERENCES reports (id),
    FOREIGN KEY (report_unit_id) REFERENCES report_units (id)
);

CREATE INDEX report_report_units_report_id_index ON report_report_units (report_id);
CREATE INDEX report_report_units_report_unit_id_index ON report_report_units (report_unit_id);

CREATE TABLE report_units
(
    id   BLOB PRIMARY KEY, -- BLAKE3 hash of the report unit data (256 bits)
    data BLOB NOT NULL     -- Serialized report unit data
);
