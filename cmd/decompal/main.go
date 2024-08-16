package main

import (
	"context"
	"github.com/encounter/decompal/config"
	"github.com/encounter/decompal/handlers"
	"github.com/gregjones/httpcache"
	"github.com/palantir/go-baseapp/baseapp"
	"github.com/palantir/go-githubapp/githubapp"
	"github.com/pkg/errors"
	"github.com/rs/zerolog"
	"goji.io/pat"
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

	// Configure a temporary directory
	//if cfg.App.TmpDir == "" {
	//	cfg.App.TmpDir, err = os.MkdirTemp(os.TempDir(), "decompal")
	//	if err != nil {
	//		logger.Fatal().
	//			Err(err).
	//			Str("path", cfg.App.TmpDir).
	//			Msg("failed to create temporary directory")
	//	}
	//}
	//if _, err := os.Stat(cfg.App.TmpDir); err != nil {
	//	if os.IsNotExist(err) {
	//		err := os.MkdirAll(cfg.App.TmpDir, 0755)
	//		if err != nil {
	//			logger.Fatal().
	//				Err(err).
	//				Str("path", cfg.App.TmpDir).
	//				Msg("failed to create temporary directory")
	//		}
	//	} else {
	//		logger.Fatal().
	//			Err(err).
	//			Str("path", cfg.App.TmpDir).
	//			Msg("failed to stat temporary directory")
	//	}
	//}
	//logger.Debug().
	//	Str("path", cfg.App.TmpDir).
	//	Msg("Using temporary directory")
	//// Delete the temporary directory on exit
	//defer func() {
	//	if err := os.RemoveAll(cfg.App.TmpDir); err != nil {
	//		logger.Error().
	//			Err(err).
	//			Str("path", cfg.App.TmpDir).
	//			Msg("failed to remove temporary directory")
	//	}
	//}()

	// Create a task queue
	//queueFactory := memqueue.NewFactory()
	//taskQueue := queueFactory.RegisterQueue(&taskq.QueueOptions{
	//	Name: "background-tasks",
	//})

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

	// Register GitHub webhook handlers
	taskCtx, taskCancel := context.WithCancel(context.Background())
	defer taskCancel()
	server.Mux().Handle(pat.Post(githubapp.DefaultWebhookRoute), githubapp.NewDefaultEventDispatcher(
		cfg.GitHub,
		handlers.NewPullRequestHandler(cc, &cfg.App, taskCtx),
		handlers.NewWorkflowRunHandler(cc, &cfg.App, taskCtx),
	))

	// Start the server (blocking)
	if err = server.Start(); err != nil {
		logger.Fatal().Err(err).Msg("server failed")
	}
}
