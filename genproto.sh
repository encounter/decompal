#!/bin/sh -e
SRC_DIR=../objdiff/objdiff-core/protos
protoc -I=$SRC_DIR \
  --go_out=paths=source_relative:objdiff \
  --go_opt=Mreport.proto=github.com/encounter/decompal/objdiff \
  $SRC_DIR/report.proto
