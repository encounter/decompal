package common

import "time"

type Project struct {
	ID    int64
	Owner string
	Name  string
}

func (p *Project) URL() string {
	return "https://github.com/" + p.Owner + "/" + p.Name
}

type Commit struct {
	Sha       string
	Timestamp time.Time
}
