ALTER TABLE report_units ADD COLUMN name TEXT;
CREATE INDEX report_units_name_idx ON report_units (name);
