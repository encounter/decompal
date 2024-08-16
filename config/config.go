package config

import (
	"github.com/palantir/go-baseapp/baseapp"
	"github.com/palantir/go-githubapp/githubapp"
	"github.com/pkg/errors"
	"gopkg.in/yaml.v3"
	"os"
)

type Config struct {
	Server  baseapp.HTTPConfig    `yaml:"server"`
	Logging baseapp.LoggingConfig `yaml:"logging"`
	GitHub  githubapp.Config      `yaml:"github"`
	App     AppConfig             `yaml:"app"`
}

type AppConfig struct {
	//TmpDir      string `yaml:"tmp_dir"`
	ObjdiffPath string `yaml:"objdiff_path"`
}

func ReadConfig(path string) (Config, error) {
	var c Config

	bytes, err := os.ReadFile(path)
	if err != nil {
		return c, errors.Wrapf(err, "failed reading server config file: %s", path)
	}

	if err := yaml.Unmarshal(bytes, &c); err != nil {
		return c, errors.Wrap(err, "failed parsing configuration file")
	}

	return c, nil
}
