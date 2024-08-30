#!/bin/sh -e
SRC_DIR=../objdiff/objdiff-core/protos
protoc -I=$SRC_DIR \
  --go_out=paths=source_relative:common \
  --go_opt=Mreport.proto=github.com/encounter/decompal/common \
  $SRC_DIR/report.proto
