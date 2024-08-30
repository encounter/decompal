package common

import (
	"github.com/klauspost/compress/zstd"
	"github.com/pkg/errors"
	"github.com/zeebo/blake3"
	"google.golang.org/protobuf/proto"
)

type SerializedUnitKey [32]byte

type SerializedReport struct {
	Data  []byte
	Units []SerializedReportUnit
}

type SerializedReportUnit struct {
	Key  SerializedUnitKey
	Data []byte
}

func (report *Report) Serialize() (*SerializedReport, error) {
	sparseReport := &Report{
		Measures: report.Measures,
		Units:    []*ReportUnit{},
	}
	options := proto.MarshalOptions{
		Deterministic: true,
	}
	data, err := options.Marshal(sparseReport)
	if err != nil {
		return nil, err
	}
	result := &SerializedReport{
		Data:  data,
		Units: make([]SerializedReportUnit, 0, len(report.Units)),
	}
	for _, unit := range report.Units {
		bytes, err := options.Marshal(unit)
		if err != nil {
			return nil, err
		}
		hash := blake3.Sum512(bytes)
		compressed, err := compress(bytes)
		if err != nil {
			return nil, err
		}
		serialized := SerializedReportUnit{Data: compressed}
		copy(serialized.Key[:], hash[:])
		result.Units = append(result.Units, serialized)
	}
	return result, nil
}

func (serialized *SerializedReport) Deserialize() (*Report, error) {
	sparseReport := &Report{}
	options := proto.UnmarshalOptions{}
	if err := options.Unmarshal(serialized.Data, sparseReport); err != nil {
		return nil, err
	}
	report := &Report{
		Measures: sparseReport.Measures,
		Units:    make([]*ReportUnit, 0, len(serialized.Units)),
	}
	for _, unit := range serialized.Units {
		bytes, err := decompress(unit.Data)
		if err != nil {
			return nil, err
		}
		key := SerializedUnitKey{}
		hash := blake3.Sum512(bytes)
		copy(key[:], hash[:])
		if key != unit.Key {
			return nil, errors.New("unit key mismatch")
		}
		unitData := &ReportUnit{}
		if err := options.Unmarshal(bytes, unitData); err != nil {
			return nil, err
		}
		report.Units = append(report.Units, unitData)
	}
	return report, nil
}

var encoder, _ = zstd.NewWriter(nil, zstd.WithEncoderLevel(zstd.SpeedFastest))
var decoder, _ = zstd.NewReader(nil)

func compress(in []byte) ([]byte, error) {
	dst := make([]byte, 0 /*, encoder.MaxEncodedLen(len(in))*/)
	dst = encoder.EncodeAll(in, dst)
	return dst, nil
}

func decompress(in []byte) ([]byte, error) {
	header := zstd.Header{}
	if err := header.Decode(in); err != nil {
		// Assume the data is not compressed
		return in, nil
	}
	capacity := uint64(0)
	if header.HasFCS {
		capacity = header.FrameContentSize
	}
	dst := make([]byte, 0, capacity)
	var err error
	if dst, err = decoder.DecodeAll(in, dst); err != nil {
		return nil, err
	}
	return dst, nil
}
