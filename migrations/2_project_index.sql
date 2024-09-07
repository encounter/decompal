CREATE INDEX projects_owner_name_index ON projects (owner COLLATE NOCASE, name COLLATE NOCASE);
