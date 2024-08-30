package database

import (
	"context"
	"database/sql"
	"errors"
	"github.com/encounter/decompal/common"
)

func (d *DB) InsertReport(ctx context.Context, file *common.ReportFile) error {
	serialized, err := file.Report.Serialize()
	if err != nil {
		return err
	}
	tx, err := d.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err = UpsertProject(tx, ctx, file.Project); err != nil {
		return err
	}
	for _, unit := range serialized.Units {
		err := insertReportUnits(tx, ctx, unit)
		if err != nil {
			return err
		}
	}
	row := tx.QueryRowContext(
		ctx,
		`INSERT INTO reports (project_id, version, git_commit, timestamp, data)
		 VALUES (?, ?, ?, ?, ?) 
		 ON CONFLICT (project_id, version, git_commit) DO UPDATE SET timestamp = EXCLUDED.timestamp
		 RETURNING id`,
		file.Project.ID, file.Version, file.Commit.Sha, file.Commit.Timestamp, serialized.Data,
	)
	var reportID int64
	if err = row.Scan(&reportID); err != nil {
		return err
	}
	for idx, unit := range serialized.Units {
		_, err = tx.ExecContext(
			ctx,
			`INSERT INTO report_report_units (report_id, report_unit_id, unit_index) VALUES (?, ?, ?)
			 	   ON CONFLICT (report_id, report_unit_id) DO NOTHING`,
			reportID, unit.Key[:], idx,
		)
		if err != nil {
			return err
		}
	}
	return tx.Commit()
}

func insertReportUnits(tx *sql.Tx, ctx context.Context, unit common.SerializedReportUnit) error {
	_, err := tx.ExecContext(
		ctx,
		`INSERT INTO report_units (id, data) VALUES (?, ?) ON CONFLICT(id) DO NOTHING`,
		unit.Key[:], unit.Data,
	)
	return err
}

func (d *DB) ReportExists(ctx context.Context, projectID int64, version string, commitSha string) (bool, error) {
	row := d.QueryRowContext(
		ctx,
		`SELECT EXISTS(
			SELECT 1 FROM reports
			WHERE project_id = ? AND version = ? AND git_commit = ?
		)`,
		projectID, version, commitSha,
	)
	var exists bool
	if err := row.Scan(&exists); err != nil {
		return false, err
	}
	return exists, nil
}

func (d *DB) GetReport(ctx context.Context, projectID int64, version string, commitSha string) (*common.ReportFile, error) {
	row := d.QueryRowContext(
		ctx,
		`SELECT r.id, r.timestamp, r.data, p.name, p.owner
			   FROM reports r JOIN projects p ON r.project_id = p.id
			   WHERE p.id = ? AND r.version = ? AND r.git_commit = ?`,
		projectID, version, commitSha,
	)
	var reportID int64
	commit := &common.Commit{
		Sha: commitSha,
	}
	var reportData []byte
	project := &common.Project{
		ID: projectID,
	}
	if err := row.Scan(&reportID, &commit.Timestamp, &reportData, &project.Name, &project.Owner); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return nil, nil
		}
		return nil, err
	}
	serialized := &common.SerializedReport{
		Data: reportData,
	}
	rows, err := d.QueryContext(
		ctx,
		`SELECT ru.id, ru.data
			   FROM report_report_units rru
			   JOIN report_units ru on ru.id = rru.report_unit_id
			   WHERE report_id = ?
			   ORDER BY rru.unit_index`,
		reportID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	for rows.Next() {
		var keyBytes []byte
		var data []byte
		if err = rows.Scan(&keyBytes, &data); err != nil {
			return nil, err
		}
		var key common.SerializedUnitKey
		if len(keyBytes) != len(key) {
			return nil, errors.New("invalid key length")
		}
		copy(key[:], keyBytes)
		serialized.Units = append(serialized.Units, common.SerializedReportUnit{
			Key:  key,
			Data: data,
		})
	}
	report, err := serialized.Deserialize()
	if err != nil {
		return nil, err
	}
	return &common.ReportFile{
		Project: project,
		Version: version,
		Commit:  commit,
		Report:  report,
	}, nil
}
