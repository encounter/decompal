package handlers

import (
	"context"
	"encoding/json"
	"github.com/encounter/decompal/common"
	"github.com/encounter/decompal/config"
	"github.com/encounter/decompal/database"
	"github.com/encounter/decompal/objdiff"
	"github.com/google/go-github/v63/github"
	"github.com/palantir/go-githubapp/githubapp"
	"github.com/pkg/errors"
)

type pullRequestHandler struct {
	githubapp.ClientCreator
	config  *config.AppConfig
	db      *database.DB
	taskCtx context.Context
}

func NewPullRequestHandler(
	cc githubapp.ClientCreator,
	config *config.AppConfig,
	db *database.DB,
	taskCtx context.Context,
) githubapp.EventHandler {
	return &pullRequestHandler{
		ClientCreator: cc,
		config:        config,
		db:            db,
		taskCtx:       taskCtx,
	}
}

func (h *pullRequestHandler) Handles() []string {
	return []string{"pull_request"}
}

func (h *pullRequestHandler) Handle(_ context.Context, eventType, deliveryID string, payload []byte) error {
	event := &github.PullRequestEvent{}
	if err := json.Unmarshal(payload, event); err != nil {
		return errors.Wrap(err, "failed to parse pull request event payload")
	}
	if event.GetAction() != "opened" {
		return nil
	}

	installationID := githubapp.GetInstallationIDFromEvent(event)
	client, err := h.NewInstallationClient(installationID)
	if err != nil {
		return err
	}

	go backgroundTask(h.taskCtx, eventType, deliveryID, func(ctx context.Context) error {
		repo := event.GetRepo()
		// Re-prepare logger instead of using the one from the request context
		ctx, logger := githubapp.PrepareRepoContext(ctx, installationID, repo)

		// Find any completed workflow runs for the current PR
		project := &common.Project{
			ID:    repo.GetID(),
			Owner: repo.GetOwner().GetLogin(),
			Name:  repo.GetName(),
		}
		pr := event.GetPullRequest()
		ghc, _, err := client.Git.GetCommit(ctx, project.Owner, project.Name, pr.GetHead().GetSHA())
		if err != nil {
			return errors.Wrap(err, "failed to get commit")
		}
		commit := &common.Commit{
			Sha:       ghc.GetSHA(),
			Timestamp: ghc.GetCommitter().GetDate().Time,
		}
		runs, _, err := client.Actions.ListRepositoryWorkflowRuns(
			ctx,
			project.Owner,
			project.Name,
			&github.ListWorkflowRunsOptions{
				Status:              "completed",
				HeadSHA:             commit.Sha,
				ExcludePullRequests: true,
			},
		)
		if err != nil {
			return errors.Wrap(err, "failed to list workflow runs")
		}
		if len(runs.WorkflowRuns) == 0 {
			logger.Debug().Msg("No workflow runs found")
			return nil
		}

		// Find report files in any completed workflow runs
		var files []common.ReportFile
		var run *github.WorkflowRun
		for _, run = range runs.WorkflowRuns {
			files, err = objdiff.FetchReportFiles(
				ctx,
				h.db,
				logger,
				client,
				project,
				commit,
				run.GetID(),
			)
			if err != nil {
				return err
			}
			if len(files) > 0 {
				break
			}
		}
		if run == nil || len(files) == 0 {
			logger.Info().Msg("No report files found")
			return nil
		}

		// Generate changes and create a PR comment
		err = processPR(
			ctx,
			h.db,
			h.config,
			installationID,
			pr,
			commit,
			client,
			repo,
			run.GetWorkflowID(),
			files,
		)
		if err != nil {
			return err
		}

		return nil
	})

	return nil
}
