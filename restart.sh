#!/bin/bash
toolforge jobs delete bot
\rm ~/bot.*
toolforge jobs run --mem 3500Mi --cpu 2 --continuous \
	--mount=all \
	--image tool-quickstatements/tool-quickstatements:latest \
	--command "target/release/main --command bot --config-file /data/project/quickstatements/rust/quickstatements_rs/config_rs.json" \
	--filelog -o /data/project/quickstatements/bot.out -e /data/project/quickstatements/bot.err \
	bot
