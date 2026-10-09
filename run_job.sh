#!/bin/bash
# Runs main as a one-off Toolforge job, passing on all arguments, e.g.
#   ./run_job.sh migrate_copy
#   ./run_job.sh restore --id 12345
# Uses the image built by build.sh. Output: /data/project/quickstatements/qs-<command>.{out,err}
set -euo pipefail

if [ $# -lt 1 ]; then
	echo "Usage: $0 <command> [more arguments for main]" >&2
	exit 1
fi
command=$1
shift
job="qs-${command//_/-}"
config=/data/project/quickstatements/rust/quickstatements_rs/config_rs.json
logs=/data/project/quickstatements

# Never replace a job that may still be running
if toolforge jobs list | grep -qw -- "$job"; then
	echo "Job $job exists, running or finished. Check: toolforge jobs show $job" >&2
	echo "Remove it once finished: toolforge jobs delete $job" >&2
	exit 1
fi

if [ "$command" = migrate_swap ]; then
	read -r -p "Are the bot (toolforge jobs delete bot) and the web service stopped? [y/N] " answer
	[ "$answer" = y ] || exit 1
fi

args=""
if [ $# -gt 0 ]; then
	args=$(printf ' %q' "$@")
fi

toolforge jobs run --mem 2Gi --cpu 1 --mount=all \
	--image tool-quickstatements/tool-quickstatements:latest \
	--command "target/release/main --command $command --config-file $config$args" \
	--filelog -o "$logs/$job.out" -e "$logs/$job.err" \
	"$job"
echo "Started $job. Follow: tail -f $logs/$job.out $logs/$job.err"
