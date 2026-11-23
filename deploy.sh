#!/usr/bin/env bash
# Delopy and restart toolforge services
ssh login.toolforge.org 'become quickstatements bash -lc "cd ~/rust/quickstatements_rs/ && git pull && ./build.sh && ./restart.sh && toolforge webservice restart && echo \"OK\""'
