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

type workflowRunHandler struct {
	githubapp.ClientCreator
	config  *config.AppConfig
	taskCtx context.Context
}

func NewWorkflowRunHandler(
	cc githubapp.ClientCreator,
	config *config.AppConfig,
	taskCtx context.Context,
) githubapp.EventHandler {
	return &workflowRunHandler{
		ClientCreator: cc,
		config:        config,
		taskCtx:       taskCtx,
	}
}

func (h *workflowRunHandler) Handles() []string {
	return []string{"workflow_run"}
}

func (h *workflowRunHandler) Handle(ctx context.Context, eventType, deliveryID string, payload []byte) error {
	event := &github.WorkflowRunEvent{}
	if err := json.Unmarshal(payload, event); err != nil {
		return errors.Wrap(err, "failed to parse workflow run event payload")
	}

	installationID := githubapp.GetInstallationIDFromEvent(event)
	ctx, logger := githubapp.PrepareRepoContext(ctx, installationID, event.GetRepo())
	status := event.GetWorkflowRun().GetStatus()
	if status != "completed" {
		logger.Debug().
			Str("status", status).
			Msg("Workflow run event is not completed")
		return nil
	}

	client, err := h.NewInstallationClient(installationID)
	if err != nil {
		return err
	}

	go backgroundTask(h.taskCtx, eventType, deliveryID, func(ctx context.Context) error {
		repo := event.GetRepo()
		// Re-prepare logger instead of using the one from the request context
		ctx, logger := githubapp.PrepareRepoContext(ctx, installationID, repo)

		// Fetch report files for the current workflow run
		repoOwner := repo.GetOwner().GetLogin()
		repoName := repo.GetName()
		run := event.GetWorkflowRun()
		runId := run.GetID()
		sha := run.GetHeadSHA()
		files, err := objdiff.FetchReportFiles(
			ctx,
			logger,
			client,
			repoOwner,
			repoName,
			sha,
			runId,
		)
		if err != nil {
			return err
		}
		if len(files) == 0 {
			logger.Info().Msg("No report files found")
			return nil
		}

		// Process all pull requests associated with the workflow run
		prs := event.GetWorkflowRun().PullRequests
		if prs != nil {
			for _, pr := range prs {
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
			}
		}

		return nil
	})

	return nil
}
