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
