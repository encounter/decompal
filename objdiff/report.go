package objdiff

import (
	"context"
	"github.com/encounter/decompal/common"
	"github.com/encounter/decompal/database"
	"github.com/encounter/decompal/zipstream"
	"github.com/google/go-github/v63/github"
	"github.com/pkg/errors"
	"github.com/rs/zerolog"
	"google.golang.org/protobuf/proto"
	"io"
	"net/http"
	"regexp"
	"sort"
	"strings"
	"time"
)

var artifactNameRegex = regexp.MustCompile(`^(?P<version>[A-z0-9_\-]+)[_-]report(?:[_-].*)?$`)

func FetchReportFiles(
	ctx context.Context,
	db *database.DB,
	logger zerolog.Logger,
	client *github.Client,
	project *common.Project,
	commit *common.Commit,
	runId int64,
) ([]common.ReportFile, error) {
	logger = logger.With().
		Str("commit_sha", commit.Sha).
		Int64("workflow_run_id", runId).
		Logger()

	artifacts, _, err := client.Actions.ListWorkflowRunArtifacts(ctx, project.Owner, project.Name, runId, nil)
	if err != nil {
		logger.Error().
			Err(err).
			Msg("Failed to list workflow run artifacts")
		return nil, errors.Wrap(err, "failed to list workflow run artifacts")
	}

	files := make([]common.ReportFile, 0)
	for _, artifact := range artifacts.Artifacts {
		logger := logger.With().
			Str("artifact_name", artifact.GetName()).
			Int64("artifact_id", artifact.GetID()).
			Logger()

		matches := artifactNameRegex.FindStringSubmatch(artifact.GetName())
		if matches == nil {
			//logger.Debug().Msg("Skipping artifact")
			continue
		}
		version := matches[artifactNameRegex.SubexpIndex("version")]

		start := time.Now()
		existing, err := db.GetReport(ctx, project.ID, version, commit.Sha)
		if err != nil {
			logger.Fatal().Err(err).Msg("failed to check if report exists")
		}
		if existing != nil {
			end := time.Now()
			logger.Info().
				Str("duration", end.Sub(start).String()).
				Msg("Report already exists")
			files = append(files, *existing)
			continue
		}

		artifactUrl, _, err := client.Actions.DownloadArtifact(ctx, project.Owner, project.Name, artifact.GetID(), 3)
		if err != nil {
			return nil, errors.Wrap(err, "failed to get artifact download url")
		}

		req, err := http.NewRequestWithContext(ctx, http.MethodGet, artifactUrl.String(), nil)
		if err != nil {
			return nil, errors.Wrap(err, "failed to create download request")
		}

		req.Header.Set("User-Agent", client.UserAgent)
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			return nil, errors.Wrap(err, "failed to download artifact")
		}

		report, err := findReportFile(logger, resp.Body)
		_ = resp.Body.Close()
		if err != nil {
			return nil, err
		}
		if report != nil {
			file := common.ReportFile{
				Project: project,
				Version: version,
				Commit:  commit,
				Report:  report,
			}
			start := time.Now()
			if err = db.InsertReport(ctx, &file); err != nil {
				return nil, errors.Wrap(err, "failed to insert report")
			}
			end := time.Now()
			logger.Info().
				Str("duration", end.Sub(start).String()).
				Msg("Inserted report")
			files = append(files, file)
		}
	}

	// Sort files by version
	sort.Slice(files, func(i, j int) bool {
		return files[i].Version < files[j].Version
	})
	return files, nil
}

// findReportFile reads the zip stream and writes the report file to the output path
// Returns true if the report file was found and written
func findReportFile(logger zerolog.Logger, r io.Reader) (*common.Report, error) {
	zr := zipstream.NewReader(r)
	for {
		entry, err := zr.Next()
		if err != nil {
			if err == io.EOF {
				break
			}
			return nil, errors.Wrap(err, "failed to get next entry")
		}

		data, err := io.ReadAll(entry)
		if err != nil {
			return nil, errors.Wrap(err, "failed to read report file")
		}
		if strings.HasSuffix(entry.Name, "report.json") {
			report := &common.Report{}
			err := common.ParseReportJson(data, report)
			if err != nil {
				return nil, errors.Wrap(err, "failed to read report file")
			}
			logger.Info().
				Str("filename", entry.Name).
				Msg("Read report file")
			return report, nil
		} else if strings.HasSuffix(entry.Name, "report.binpb") ||
			strings.HasSuffix(entry.Name, "report.pb") {
			report := &common.Report{}
			err = proto.Unmarshal(data, report)
			if err != nil {
				return nil, err
			}
			logger.Info().
				Str("filename", entry.Name).
				Msg("Read report file")
			return report, nil
		}
	}
	return nil, nil
}
