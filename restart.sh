#!/bin/bash
toolforge jobs delete bot
toolforge jobs run --mem 5000Mi --cpu 3 --continuous --mount=all --image tool-quickstatements/tool-quickstatements:latest --command "sh -c 'target/release/bot --command bot --config-file /data/project/quickstatements/rust/quickstatements_rs/config_rs.json'" bot
