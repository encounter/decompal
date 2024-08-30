package main

import (
	"context"
	"github.com/encounter/decompal/common"
	"github.com/encounter/decompal/config"
	"github.com/encounter/decompal/database"
	"github.com/encounter/decompal/handlers"
	"github.com/encounter/decompal/objdiff"
	"github.com/google/go-github/v63/github"
	"github.com/gregjones/httpcache"
	"github.com/palantir/go-baseapp/baseapp"
	"github.com/palantir/go-githubapp/githubapp"
	"github.com/pkg/errors"
	"github.com/rs/zerolog"
	"goji.io/pat"
	"google.golang.org/protobuf/proto"
	"time"
)

func main() {
	// Load configuration from a file
	cfg, err := config.ReadConfig("config.yml")
	if err != nil {
		panic(errors.Wrap(err, "failed to read config"))
	}
	logger := baseapp.NewLogger(cfg.Logging)
	zerolog.DefaultContextLogger = &logger

	// Open the database
	db, err := database.Open(cfg.App.DbPath)
	if err != nil {
		logger.Fatal().Err(err).Msg("failed to open database")
	}
	defer db.Close()

	// Create the server
	serverParams := baseapp.DefaultParams(logger, "decompal.")
	server, err := baseapp.NewServer(cfg.Server, serverParams...)
	if err != nil {
		logger.Fatal().Err(err).Msg("failed to create server")
	}

	// Create GitHub app client
	cc, err := githubapp.NewDefaultCachingClientCreator(
		cfg.GitHub,
		githubapp.WithClientUserAgent("decompal/1.0.0"),
		githubapp.WithClientTimeout(5*time.Second),
		githubapp.WithClientCaching(false, func() httpcache.Cache { return httpcache.NewMemoryCache() }),
	)
	if err != nil {
		logger.Fatal().Err(err).Msg("failed to create GitHub app client")
	}

	// --- START TESTING ---
	client, err := cc.NewTokenClient(cfg.App.GitHubToken)
	if err != nil {
		logger.Fatal().Err(err).Msg("failed to create GitHub app client")
	}
	runs := make([]*github.WorkflowRun, 0)
	page := 0
	ctx := context.Background()
	for {
		result, _, err := client.Actions.ListWorkflowRunsByFileName(ctx, "zeldaret", "tww", "build.yml", &github.ListWorkflowRunsOptions{
			Branch:              "main",
			Event:               "push",
			Status:              "completed",
			ExcludePullRequests: true,
			ListOptions: github.ListOptions{
				Page:    page,
				PerPage: 10,
			},
		})
		if err != nil {
			logger.Fatal().Err(err).Msg("failed to list workflow runs by file name")
		}
		runs = append(runs, result.WorkflowRuns...)
		logger.Info().Msgf("Found %d workflow runs", len(runs))
		found := false
		for _, run := range result.WorkflowRuns {
			if run.GetID() == 9983071101 {
				found = true
				break
			}
		}
		if found {
			break
		}
		page++
	}
	project := &common.Project{
		ID:    689343905,
		Owner: "zeldaret",
		Name:  "tww",
	}
	for _, run := range runs {
		logger.Info().Msgf("Processing workflow run %d (%s)", run.GetID(), run.GetCreatedAt().String())
		ghc, _, err := client.Git.GetCommit(ctx, project.Owner, project.Name, run.GetHeadSHA())
		if err != nil {
			logger.Fatal().Err(err).Msg("failed to get commit")
		}
		commit := &common.Commit{
			Sha:       ghc.GetSHA(),
			Timestamp: ghc.GetCommitter().GetDate().Time,
		}
		reports, err := objdiff.FetchReportFiles(ctx, db, logger, client, project, commit, run.GetID())
		if err != nil {
			logger.Fatal().Err(err).Msg("failed to fetch report files")
		}
		for _, report := range reports {
			//err = db.InsertReport(ctx, &report)
			//if err != nil {
			//	logger.Fatal().Err(err).Msg("failed to insert report")
			//}
			fetched, err := db.GetReport(ctx, report.Project.ID, report.Version, commit.Sha)
			if err != nil {
				logger.Fatal().Err(err).Msg("failed to get report")
			}
			if *project != *fetched.Project {
				logger.Fatal().Msg("fetched project does not match inserted project")
			}
			if report.Version != fetched.Version {
				logger.Fatal().Msg("fetched version does not match inserted version")
			}
			if *commit != *fetched.Commit {
				logger.Fatal().Msg("fetched commit does not match inserted commit")
			}
			if !proto.Equal(fetched.Report, report.Report) {
				if !proto.Equal(fetched.Report.Measures, report.Report.Measures) {
					logger.Error().Msg("measures do not match")
				}
				if len(fetched.Report.Units) != len(report.Report.Units) {
					logger.Error().Msgf("unit count does not match %d != %d", len(fetched.Report.Units), len(report.Report.Units))
				} else {
					for i, unit := range report.Report.Units {
						if !proto.Equal(fetched.Report.Units[i], unit) {
							logger.Error().Msgf("unit %d does not match", i)
						}
					}
				}
				logger.Fatal().Msg("fetched report does not match inserted report")
			}
		}
		if run.GetID() == 9983071101 {
			logger.Info().Msg("Stopping at run 9983071101")
			break
		}
	}
	// --- END TESTING ---

	// Register GitHub webhook handlers
	taskCtx, taskCancel := context.WithCancel(ctx)
	defer taskCancel()
	server.Mux().Handle(pat.Post(githubapp.DefaultWebhookRoute), githubapp.NewDefaultEventDispatcher(
		cfg.GitHub,
		handlers.NewPullRequestHandler(cc, &cfg.App, db, taskCtx),
		handlers.NewWorkflowRunHandler(cc, &cfg.App, db, taskCtx),
	))

	// Start the server (blocking)
	if err = server.Start(); err != nil {
		logger.Fatal().Err(err).Msg("server failed")
	}
}
