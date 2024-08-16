package handlers

import (
	"context"
	"github.com/palantir/go-githubapp/githubapp"
	"github.com/rs/zerolog"
	"time"
)

func backgroundTask(taskCtx context.Context, eventType, deliveryID string, run func(context.Context) error) {
	logger := zerolog.Ctx(taskCtx).With().
		Str(githubapp.LogKeyDeliveryID, deliveryID).
		Str(githubapp.LogKeyEventType, eventType).
		Logger()
	ctx, cancel := context.WithDeadline(taskCtx, time.Now().Add(time.Minute))
	defer cancel()
	ctx = logger.WithContext(ctx)
	err := run(ctx)
	if err != nil {
		logger.Error().Err(err).Msg("Background task failed")
	}
}
