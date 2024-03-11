#!/bin/bash
toolforge jobs delete single

toolforge jobs run --wait --mem 2000Mi --cpu 1 --mount=all --image tool-listeria/tool-listeria:latest \
	--command "sh -c 'target/release/bot --command $1 --config-file /data/project/quickstatements/rust/quickstatements_rs/config_rs.json'" single

toolforge jobs logs single
