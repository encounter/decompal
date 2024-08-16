package handlers

import (
	"context"
	"encoding/json"
	"github.com/encounter/decompal/config"
	"github.com/encounter/decompal/objdiff"
	"github.com/google/go-github/v63/github"
	"github.com/palantir/go-githubapp/githubapp"
	"github.com/pkg/errors"
)

type pullRequestHandler struct {
	githubapp.ClientCreator
	config  *config.AppConfig
	taskCtx context.Context
}

func NewPullRequestHandler(
	cc githubapp.ClientCreator,
	config *config.AppConfig,
	taskCtx context.Context,
) githubapp.EventHandler {
	return &pullRequestHandler{
		ClientCreator: cc,
		config:        config,
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
		repoOwner := repo.GetOwner().GetLogin()
		repoName := repo.GetName()
		pr := event.GetPullRequest()
		sha := pr.GetHead().GetSHA()
		runs, _, err := client.Actions.ListRepositoryWorkflowRuns(
			ctx,
			repoOwner,
			repoName,
			&github.ListWorkflowRunsOptions{
				Status:              "completed",
				HeadSHA:             sha,
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
		var files []objdiff.ReportFile
		var run *github.WorkflowRun
		for _, run = range runs.WorkflowRuns {
			files, err = objdiff.FetchReportFiles(
				ctx,
				logger,
				client,
				repoOwner,
				repoName,
				sha,
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
			h.config,
			installationID,
			pr,
			sha,
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
