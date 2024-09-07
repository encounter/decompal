DROP INDEX reports_project_id_version_git_commit_index;
CREATE UNIQUE INDEX reports_project_id_version_git_commit_index ON reports (project_id, version COLLATE NOCASE, git_commit COLLATE NOCASE);

DROP INDEX reports_timestamp_index;
CREATE INDEX reports_project_id_timestamp_index ON reports (project_id, timestamp);

CREATE INDEX project_owner_name_index ON projects (owner COLLATE NOCASE, name COLLATE NOCASE);
