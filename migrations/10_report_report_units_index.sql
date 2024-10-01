PRAGMA foreign_keys = off;

ALTER TABLE report_report_units
    RENAME TO report_report_units_old;

CREATE TABLE report_report_units
(
    report_id      INTEGER NOT NULL,
    report_unit_id BLOB    NOT NULL,
    unit_index     INTEGER NOT NULL, -- Index of the report unit in the report
    PRIMARY KEY (report_id, report_unit_id, unit_index),
    FOREIGN KEY (report_id) REFERENCES reports (id),
    FOREIGN KEY (report_unit_id) REFERENCES report_units (id)
);

DROP INDEX report_report_units_report_id_index;
DROP INDEX report_report_units_report_unit_id_index;

CREATE INDEX report_report_units_report_id_index ON report_report_units (report_id);
CREATE INDEX report_report_units_report_unit_id_index ON report_report_units (report_unit_id);

INSERT INTO report_report_units
SELECT *
FROM report_report_units_old;

DROP TABLE report_report_units_old;

PRAGMA foreign_keys = on;