package objdiff

import (
	"context"
	"encoding/json"
	"github.com/encounter/decompal/zipstream"
	"github.com/google/go-github/v63/github"
	"github.com/pkg/errors"
	"github.com/rs/zerolog"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
	"io"
	"net/http"
	"regexp"
	"sort"
	"strings"
)

type ReportFile struct {
	Version string
	Sha     string
	Report  *Report
}

var artifactNameRegex = regexp.MustCompile(`^(?P<version>[A-z0-9_\-]+)_report$`)

func FetchReportFiles(
	ctx context.Context,
	logger zerolog.Logger,
	client *github.Client,
	repoOwner, repoName, sha string,
	runId int64,
) ([]ReportFile, error) {
	logger = logger.With().
		Str("commit_sha", sha).
		Int64("workflow_run_id", runId).
		Logger()

	artifacts, _, err := client.Actions.ListWorkflowRunArtifacts(ctx, repoOwner, repoName, runId, nil)
	if err != nil {
		logger.Error().
			Err(err).
			Msg("Failed to list workflow run artifacts")
		return nil, errors.Wrap(err, "failed to list workflow run artifacts")
	}

	files := make([]ReportFile, 0)
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

		artifactUrl, _, err := client.Actions.DownloadArtifact(ctx, repoOwner, repoName, artifact.GetID(), 3)
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
			files = append(files, ReportFile{
				Version: version,
				Sha:     sha,
				Report:  report,
			})
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
func findReportFile(logger zerolog.Logger, r io.Reader) (*Report, error) {
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
			report := &Report{}
			err := parseJson(data, report)
			if err != nil {
				return nil, errors.Wrap(err, "failed to read report file")
			}
			logger.Info().
				Str("filename", entry.Name).
				Msg("Read report file")
			return report, nil
		} else if strings.HasSuffix(entry.Name, "report.binpb") ||
			strings.HasSuffix(entry.Name, "report.pb") {
			report := &Report{}
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

func parseJson(data []byte, v *Report) error {
	err := protojson.Unmarshal(data, v)
	if err != nil {
		// Try to parse as legacy report
		legacy := &legacyReport{}
		if other := json.Unmarshal(data, legacy); other != nil {
			// Return the original error
			return err
		}
		*v = *legacy.convert()
	}
	return nil
}
