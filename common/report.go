package common

import (
	"encoding/json"
	"google.golang.org/protobuf/encoding/protojson"
)

type ReportFile struct {
	Project *Project
	Version string
	Commit  *Commit
	Report  *Report
}

func ParseReportJson(data []byte, v *Report) error {
	err := protojson.Unmarshal(data, v)
	if err != nil {
		// Try to parse as legacy report
		legacy := &legacyReport{}
		if other := json.Unmarshal(data, legacy); other != nil {
			// Return the original error
			return err
		}
		*v = *legacy.convert()
	}
	return nil
}

func (m *Measures) CalcMatchedPercent() {
	m.MatchedCodePercent = 100
	if m.TotalCode != 0 {
		m.MatchedCodePercent = float32(m.MatchedCode) / float32(m.TotalCode) * 100
	}
	m.MatchedDataPercent = 100
	if m.TotalData != 0 {
		m.MatchedDataPercent = float32(m.MatchedData) / float32(m.TotalData) * 100
	}
	m.MatchedFunctionsPercent = 100
	if m.TotalFunctions != 0 {
		m.MatchedFunctionsPercent = float32(m.MatchedFunctions) / float32(m.TotalFunctions) * 100
	}
}
