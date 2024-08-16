package objdiff

import (
	"strconv"
	"strings"
)

// Older JSON report types
type legacyReport struct {
	FuzzyMatchPercent       float32            `json:"fuzzy_match_percent"`
	TotalCode               uint64             `json:"total_code"`
	MatchedCode             uint64             `json:"matched_code"`
	MatchedCodePercent      float32            `json:"matched_code_percent"`
	TotalData               uint64             `json:"total_data"`
	MatchedData             uint64             `json:"matched_data"`
	MatchedDataPercent      float32            `json:"matched_data_percent"`
	TotalFunctions          uint32             `json:"total_functions"`
	MatchedFunctions        uint32             `json:"matched_functions"`
	MatchedFunctionsPercent float32            `json:"matched_functions_percent"`
	Units                   []legacyReportUnit `json:"units"`
}

type legacyReportUnit struct {
	Name              string             `json:"name"`
	FuzzyMatchPercent float32            `json:"fuzzy_match_percent"`
	TotalCode         uint64             `json:"total_code"`
	MatchedCode       uint64             `json:"matched_code"`
	TotalData         uint64             `json:"total_data"`
	MatchedData       uint64             `json:"matched_data"`
	TotalFunctions    uint32             `json:"total_functions"`
	MatchedFunctions  uint32             `json:"matched_functions"`
	Complete          *bool              `json:"complete,omitempty"`
	ModuleName        *string            `json:"module_name,omitempty"`
	ModuleID          *uint32            `json:"module_id,omitempty"`
	Sections          []legacyReportItem `json:"sections"`
	Functions         []legacyReportItem `json:"functions"`
}

type legacyReportItem struct {
	Name              string  `json:"name"`
	DemangledName     *string `json:"demangled_name,omitempty"`
	Address           *string `json:"address,omitempty"` // hex string
	Size              uint64  `json:"size"`
	FuzzyMatchPercent float32 `json:"fuzzy_match_percent"`
}

func (r *legacyReport) convert() *Report {
	report := &Report{
		FuzzyMatchPercent:       r.FuzzyMatchPercent,
		TotalCode:               r.TotalCode,
		MatchedCode:             r.MatchedCode,
		MatchedCodePercent:      r.MatchedCodePercent,
		TotalData:               r.TotalData,
		MatchedData:             r.MatchedData,
		MatchedDataPercent:      r.MatchedDataPercent,
		TotalFunctions:          r.TotalFunctions,
		MatchedFunctions:        r.MatchedFunctions,
		MatchedFunctionsPercent: r.MatchedFunctionsPercent,
		Units:                   make([]*ReportUnit, 0, len(r.Units)),
	}
	for _, unit := range r.Units {
		report.Units = append(report.Units, unit.convert())
	}
	return report
}

func (u *legacyReportUnit) convert() *ReportUnit {
	unit := &ReportUnit{
		Name:              u.Name,
		FuzzyMatchPercent: u.FuzzyMatchPercent,
		TotalCode:         u.TotalCode,
		MatchedCode:       u.MatchedCode,
		TotalData:         u.TotalData,
		MatchedData:       u.MatchedData,
		TotalFunctions:    u.TotalFunctions,
		MatchedFunctions:  u.MatchedFunctions,
		Complete:          u.Complete,
		ModuleName:        u.ModuleName,
		ModuleId:          u.ModuleID,
		Sections:          make([]*ReportItem, 0, len(u.Sections)),
		Functions:         make([]*ReportItem, 0, len(u.Functions)),
	}
	for _, section := range u.Sections {
		unit.Sections = append(unit.Sections, section.convert())
	}
	for _, function := range u.Functions {
		unit.Functions = append(unit.Functions, function.convert())
	}
	return unit
}

func (i *legacyReportItem) convert() *ReportItem {
	var address uint64
	if i.Address != nil {
		addressStr := *i.Address
		if strings.HasPrefix(addressStr, "0x") {
			address, _ = strconv.ParseUint(addressStr[2:], 16, 64)
		} else {
			address, _ = strconv.ParseUint(addressStr, 10, 64)
		}
	}
	return &ReportItem{
		Name:              i.Name,
		DemangledName:     i.DemangledName,
		Address:           &address,
		Size:              i.Size,
		FuzzyMatchPercent: i.FuzzyMatchPercent,
	}
}
