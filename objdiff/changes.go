package objdiff

import (
	"github.com/encounter/decompal/config"
	"github.com/pkg/errors"
	"github.com/rs/zerolog"
	"google.golang.org/protobuf/proto"
	"os/exec"
)

func GenerateChanges(
	config *config.AppConfig,
	logger zerolog.Logger,
	prev *ReportFile,
	curr *ReportFile,
) (*Changes, error) {
	if config.ObjdiffPath == "" {
		return nil, errors.New("objdiff_path not set")
	}

	data, err := proto.Marshal(&ChangesInput{
		From: prev.Report,
		To:   curr.Report,
	})
	if err != nil {
		return nil, errors.Wrap(err, "failed to encode changes input")
	}

	// Run objdiff with proto input and output
	// The `--` is to delimit the end of flags and the start of positional arguments
	// Otherwise the argument parser gets confused
	cmd := exec.Command(
		config.ObjdiffPath,
		"report",
		"changes",
		"-f",
		"proto",
		"--",
		"-",
		"-",
	)
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, errors.Wrap(err, "failed to open stdin pipe")
	}
	go func() {
		defer stdin.Close()
		if _, err := stdin.Write(data); err != nil {
			logger.Err(err).Msg("failed to write data to objdiff")
		}
	}()
	// It shouldn't have any stderr output when successful, but we want to capture errors
	// May need to change this if objdiff changes
	output, err := cmd.CombinedOutput()
	if err != nil {
		return nil, errors.Wrapf(err, "failed to generate changes: %s", string(output))
	}

	changes := &Changes{}
	err = proto.Unmarshal(output, changes)
	if err != nil {
		return nil, errors.Wrap(err, "failed to decode changes file")
	}
	return changes, nil
}
