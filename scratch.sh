#!/bin/bash
export CONDUWUIT_SERVER_NAME="nutra.tk"
export CONDUWUIT_DATABASE_PATH="/var/lib/continuwuity"
time cargo run --bin conduwuit -- yolo get-room-dag --merge-outliers --segments \!nA3iIjOXxla4QtQvED:nutra.tk 0 -1
