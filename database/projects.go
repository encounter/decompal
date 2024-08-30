package database

import (
	"context"
	"database/sql"
	"github.com/encounter/decompal/common"
)

func UpsertProject(tx *sql.Tx, ctx context.Context, project *common.Project) error {
	_, err := tx.ExecContext(
		ctx,
		`INSERT INTO projects (id, owner, name, created_at, updated_at)
			   VALUES (?, ?, ?, current_timestamp, current_timestamp)
			   ON CONFLICT(id) DO UPDATE SET owner = EXCLUDED.owner, name = EXCLUDED.name`,
		project.ID, project.Owner, project.Name,
	)
	return err
}
