package handlers

import (
	"context"
	"fmt"
	"github.com/encounter/decompal/common"
	"github.com/encounter/decompal/config"
	"github.com/encounter/decompal/database"
	"github.com/encounter/decompal/objdiff"
	"github.com/google/go-github/v63/github"
	"github.com/palantir/go-githubapp/githubapp"
	"github.com/pkg/errors"
	"strings"
)

func processPR(
	ctx context.Context,
	db *database.DB,
	config *config.AppConfig,
	installationID int64,
	pr *github.PullRequest,
	headCommit *common.Commit,
	client *github.Client,
	repo *github.Repository,
	workflowID int64,
	files []common.ReportFile,
) error {
	prNum := pr.GetNumber()
	ctx, logger := githubapp.PreparePRContext(ctx, installationID, repo, prNum)

	// Sanity check
	head := pr.GetHead()
	if head.GetSHA() != headCommit.Sha {
		logger.Debug().
			Str("head_sha", head.GetSHA()).
			Str("commit_sha", headCommit.Sha).
			Msg("Head SHA does not match workflow run SHA")
		return nil
	}

	// Find workflows runs for the PR base commit
	project := &common.Project{
		ID:    repo.GetID(),
		Owner: repo.GetOwner().GetLogin(),
		Name:  repo.GetName(),
	}
	base := pr.GetBase()
	ghc, _, err := client.Git.GetCommit(ctx, project.Owner, project.Name, base.GetSHA())
	if err != nil {
		return errors.Wrap(err, "failed to get commit")
	}
	baseCommit := &common.Commit{
		Sha:       ghc.GetSHA(),
		Timestamp: ghc.GetCommitter().GetDate().Time,
	}
	runs, _, err := client.Actions.ListWorkflowRunsByID(
		ctx,
		project.Owner,
		project.Name,
		workflowID,
		&github.ListWorkflowRunsOptions{
			Status:              "completed",
			HeadSHA:             baseCommit.Sha,
			ExcludePullRequests: true,
		},
	)
	if err != nil {
		return errors.Wrap(err, "failed to list workflow runs by file name")
	}
	if len(runs.WorkflowRuns) == 0 {
		logger.Debug().
			Str("commit_sha", baseCommit.Sha).
			Msg("No base workflow runs found")
		return nil
	}

	// Fetch report files for the PR base commit
	baseRun := runs.WorkflowRuns[0]
	baseFiles, err := objdiff.FetchReportFiles(
		ctx,
		db,
		logger,
		client,
		project,
		baseCommit,
		baseRun.GetID(),
	)
	if err != nil {
		return err
	}
	if len(baseFiles) == 0 {
		logger.Info().
			Str("commit_sha", base.GetSHA()).
			Int64("workflow_run_id", baseRun.GetID()).
			Msg("No base report files found")
		return nil
	}

	// Generate changes for each report file
	type versionChange struct {
		Version string
		Body    string
	}
	versionChanges := make([]versionChange, 0)
	for _, baseFile := range baseFiles {
		for _, file := range files {
			if baseFile.Version == file.Version {
				logger := logger.With().
					Str("version", file.Version).
					Str("from_sha", baseFile.Commit.Sha).
					Str("to_sha", file.Commit.Sha).
					Logger()
				logger.Info().Msg("Generating changes")
				changes, err := objdiff.GenerateChanges(config, logger, &baseFile, &file)
				if err != nil {
					logger.Error().Err(err).Msg("Failed to generate changes")
					return err
				}
				body := createChanges(changes)
				if body != "" {
					versionChanges = append(versionChanges, versionChange{
						Version: file.Version,
						Body:    body,
					})
				}
			}
		}
	}
	if len(versionChanges) == 0 {
		logger.Info().Msg("No changes found")
		return nil
	}

	// Generate PR comment body
	body := "## Changes\n\n"
	for _, vc := range versionChanges {
		body += fmt.Sprintf(
			"<details><summary>Version %s</summary>\n\n%s\n\n</details>\n\n",
			vc.Version,
			vc.Body,
		)
	}

	// Update or create PR comment
	err = upsertComment(ctx, client, project, prNum, body)
	if err != nil {
		return err
	}
	return nil
}

func upsertComment(
	ctx context.Context,
	client *github.Client,
	project *common.Project,
	prNum int,
	body string,
) error {
	sort := "created"
	direction := "asc"
	existing, _, err := client.Issues.ListComments(ctx, project.Owner, project.Name, prNum, &github.IssueListCommentsOptions{
		Sort:      &sort,
		Direction: &direction,
	})
	if err != nil {
		return errors.Wrap(err, "failed to list existing comments")
	}
	commentID := int64(0)
	for _, comment := range existing {
		// TODO: update go-github to expose performed_via_github_app
		if comment.GetUser().GetLogin() == "decompal[bot]" {
			commentID = comment.GetID()
			if comment.GetBody() == body {
				// No changes
				return nil
			}
			break
		}
	}
	if commentID != 0 {
		_, _, err = client.Issues.EditComment(ctx, project.Owner, project.Name, commentID, &github.IssueComment{
			Body: github.String(body),
		})
		if err != nil {
			return errors.Wrap(err, "failed to edit comment")
		}
	} else {
		_, _, err = client.Issues.CreateComment(ctx, project.Owner, project.Name, prNum, &github.IssueComment{
			Body: github.String(body),
		})
		if err != nil {
			return errors.Wrap(err, "failed to create comment")
		}
	}
	return nil
}

func createChanges(changes *common.Changes) string {
	out := "### Overall\n\n"
	overallTable := measuresTable(changes.From, changes.To)
	if overallTable == "" {
		if len(changes.Units) == 0 {
			return ""
		}
		out += "No changes\n\n"
	} else {
		out += overallTable + "\n\n"
	}
	for _, unit := range changes.Units {
		out += fmt.Sprintf("---\n### `%s`\n\n", unit.Name)
		unitTable := measuresTable(unit.From, unit.To)
		if unitTable != "" {
			out += unitTable + "\n\n"
		}
		functionsTable := changeItemTable("Functions", unit.Functions)
		if functionsTable != "" {
			out += functionsTable + "\n\n"
		}
	}
	return out
}

func changeItemTable(name string, items []*common.ChangeItem) string {
	header := fmt.Sprintf("|%s|Previous|Current|Change|\n|-|-|-|-|", name)
	rows := make([]string, 0)
	for _, item := range items {
		row := changeItemInfoRow(item)
		if row != "" {
			rows = append(rows, row)
		}
	}
	if len(rows) == 0 {
		return ""
	}
	return header + "\n" + strings.Join(rows, "\n")
}

const (
	incArrow = "${\\color{green}▲}$"
	decArrow = "${\\color{red}▼}$"
)

func floatArrow(diff float32) string {
	if diff > 0 {
		return " " + incArrow
	}
	if diff < 0 {
		return " " + decArrow
	}
	return ""
}

func intArrow(diff int64) string {
	if diff > 0 {
		return " " + incArrow
	}
	if diff < 0 {
		return " " + decArrow
	}
	return ""
}

func changeItemInfoRow(item *common.ChangeItem) string {
	var fromPercent, toPercent float32
	if item.From != nil {
		fromPercent = item.From.FuzzyMatchPercent
	}
	if item.To != nil {
		toPercent = item.To.FuzzyMatchPercent
	}
	if fromPercent == toPercent {
		return ""
	}
	diff := toPercent - fromPercent
	return fmt.Sprintf(
		"|`%s`|%.2f%%|%.2f%%|%.2f%%%s|",
		item.Name,
		fromPercent,
		toPercent,
		diff,
		floatArrow(diff),
	)
}

func measuresTable(prev, curr *common.Measures) string {
	if prev == nil && curr == nil {
		return ""
	} else if prev == nil {
		// TODO: added
		prev = &common.Measures{}
	} else if curr == nil {
		// TODO: removed
		curr = &common.Measures{}
	}
	header := "|Metric|Previous|Current|Change|\n|-|-|-|-|"
	rows := make([]string, 0)
	if prev.FuzzyMatchPercent != curr.FuzzyMatchPercent {
		rows = append(rows, floatRow("Fuzzy match", prev.FuzzyMatchPercent, curr.FuzzyMatchPercent))
	}
	if prev.TotalCode != curr.TotalCode {
		rows = append(rows, sizeRow("Total code", prev.TotalCode, curr.TotalCode))
	}
	if prev.MatchedCode != curr.MatchedCode ||
		prev.MatchedCodePercent != curr.MatchedCodePercent {
		rows = append(rows, intPercentRow(
			"Matched code",
			prev.MatchedCode,
			prev.MatchedCodePercent,
			curr.MatchedCode,
			curr.MatchedCodePercent,
		))
	}
	if prev.TotalData != curr.TotalData {
		rows = append(rows, sizeRow("Total data", prev.TotalData, curr.TotalData))
	}
	if prev.MatchedData != curr.MatchedData ||
		prev.MatchedDataPercent != curr.MatchedDataPercent {
		rows = append(rows, intPercentRow(
			"Matched data",
			prev.MatchedData,
			prev.MatchedDataPercent,
			curr.MatchedData,
			curr.MatchedDataPercent,
		))
	}
	if prev.TotalFunctions != curr.TotalFunctions {
		rows = append(rows, intRow("Total functions", prev.TotalFunctions, curr.TotalFunctions))
	}
	if prev.MatchedFunctions != curr.MatchedFunctions ||
		prev.MatchedFunctionsPercent != curr.MatchedFunctionsPercent {
		rows = append(rows, intPercentRow(
			"Matched functions",
			uint64(prev.MatchedFunctions),
			prev.MatchedFunctionsPercent,
			uint64(curr.MatchedFunctions),
			curr.MatchedFunctionsPercent,
		))
	}
	if len(rows) == 0 {
		return ""
	}
	return header + "\n" + strings.Join(rows, "\n")
}

func floatRow(name string, prev, curr float32) string {
	diff := curr - prev
	return fmt.Sprintf(
		"|%s|%.2f%%|%.2f%%|%.2f%%%s|",
		name,
		prev,
		curr,
		diff,
		floatArrow(diff),
	)
}

func intRow(name string, prev, curr uint32) string {
	diff := int64(curr) - int64(prev)
	return fmt.Sprintf(
		"|%s|%d|%d|%d%s|",
		name,
		prev,
		curr,
		diff,
		intArrow(diff),
	)
}

func sizeRow(name string, prev, curr uint64) string {
	// TODO: format size
	diff := int64(curr) - int64(prev)
	return fmt.Sprintf(
		"|%s|%d|%d|%d%s|",
		name,
		prev,
		curr,
		diff,
		intArrow(diff),
	)
}

func intPercentRow(
	name string,
	prevInt uint64,
	prevPercent float32,
	currInt uint64,
	currPercent float32,
) string {
	diff := int64(currInt) - int64(prevInt)
	return fmt.Sprintf(
		"|%s|%d (%.2f%%)|%d (%.2f%%)|%d (%.2f%%)%s|",
		name,
		prevInt,
		prevPercent,
		currInt,
		currPercent,
		diff,
		currPercent-prevPercent,
		intArrow(diff),
	)
}
